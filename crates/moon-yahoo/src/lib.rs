//! Yahoo Finance market-data client for moon-trade.

use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use moon_core::bars::Bar;
use reqwest::{StatusCode, Url};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://query1.finance.yahoo.com";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) moon-trade";
const MAX_ATTEMPTS: u32 = 3;
const SECS_PER_DAY: i64 = 86_400;

#[derive(Debug, thiserror::Error)]
pub enum YahooError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("estado HTTP {0}")]
    Status(u16),
    #[error("Yahoo respondió error {code}: {description}")]
    Api { code: String, description: String },
    #[error("respuesta inválida: {0}")]
    Parse(String),
    #[error("sin datos para {0}")]
    NoData(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct YahooQuote {
    pub symbol: String,
    pub price: f64,
    pub currency: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct YahooClient {
    http: reqwest::Client,
    base_url: String,
    retry_delay: Duration,
    max_attempts: u32,
}

impl Default for YahooClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct ChartResponse {
    chart: Chart,
}

#[derive(Debug, Deserialize)]
struct Chart {
    result: Option<Vec<ChartResult>>,
    error: Option<ChartError>,
}

#[derive(Debug, Deserialize)]
struct ChartError {
    code: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChartResult {
    #[serde(default)]
    meta: Meta,
    #[serde(default)]
    timestamp: Vec<i64>,
    #[serde(default)]
    indicators: Indicators,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    currency: Option<String>,
    gmtoffset: Option<i64>,
    regular_market_price: Option<f64>,
    regular_market_time: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct Indicators {
    #[serde(default)]
    quote: Vec<QuoteSeries>,
    #[serde(default)]
    adjclose: Vec<AdjCloseSeries>,
}

#[derive(Debug, Default, Deserialize)]
struct QuoteSeries {
    #[serde(default)]
    open: Vec<Option<f64>>,
    #[serde(default)]
    high: Vec<Option<f64>>,
    #[serde(default)]
    low: Vec<Option<f64>>,
    #[serde(default)]
    close: Vec<Option<f64>>,
    #[serde(default)]
    volume: Vec<Option<f64>>,
}

#[derive(Debug, Default, Deserialize)]
struct AdjCloseSeries {
    #[serde(default)]
    adjclose: Vec<Option<f64>>,
}

fn at(series: &[Option<f64>], i: usize) -> Option<f64> {
    series.get(i).copied().flatten()
}

fn day_start_unix(date: NaiveDate) -> i64 {
    date.and_time(chrono::NaiveTime::MIN).and_utc().timestamp()
}

impl YahooClient {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            retry_delay: Duration::from_secs(1),
            max_attempts: MAX_ATTEMPTS,
        }
    }

    pub fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay;
        self
    }

    pub async fn daily_bars(
        &self,
        symbol: &str,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<Bar>, YahooError> {
        let period1 = day_start_unix(from);
        let period2 = day_start_unix(to) + SECS_PER_DAY;
        let query = [
            ("period1", period1.to_string()),
            ("period2", period2.to_string()),
            ("interval", "1d".to_string()),
            ("events", "div,splits".to_string()),
            ("includeAdjustedClose", "true".to_string()),
        ];
        let result = self.fetch_chart(symbol, &query).await?;
        let gmtoffset = result.meta.gmtoffset.unwrap_or(0);
        let q = result
            .indicators
            .quote
            .first()
            .ok_or_else(|| YahooError::Parse("falta indicators.quote".into()))?;
        let adj = result.indicators.adjclose.first();

        let mut bars = Vec::with_capacity(result.timestamp.len());
        for (i, ts) in result.timestamp.iter().enumerate() {
            let (Some(open), Some(high), Some(low), Some(close)) = (
                at(&q.open, i),
                at(&q.high, i),
                at(&q.low, i),
                at(&q.close, i),
            ) else {
                continue;
            };
            let date = DateTime::from_timestamp(ts + gmtoffset, 0)
                .ok_or_else(|| YahooError::Parse(format!("timestamp fuera de rango: {ts}")))?
                .date_naive();
            let adj_close = adj.and_then(|a| at(&a.adjclose, i)).unwrap_or(close);
            bars.push(Bar {
                date,
                open,
                high,
                low,
                close,
                adj_close,
                volume: at(&q.volume, i).unwrap_or(0.0),
            });
        }
        Ok(bars)
    }

    pub async fn latest_quote(&self, symbol: &str) -> Result<YahooQuote, YahooError> {
        let query = [("range", "5d".to_string()), ("interval", "1d".to_string())];
        let result = self.fetch_chart(symbol, &query).await?;
        let price = result
            .meta
            .regular_market_price
            .ok_or_else(|| YahooError::Parse("falta meta.regularMarketPrice".into()))?;
        let currency = result
            .meta
            .currency
            .ok_or_else(|| YahooError::Parse("falta meta.currency".into()))?;
        let secs = result
            .meta
            .regular_market_time
            .ok_or_else(|| YahooError::Parse("falta meta.regularMarketTime".into()))?;
        let at = DateTime::from_timestamp(secs, 0)
            .ok_or_else(|| YahooError::Parse(format!("timestamp fuera de rango: {secs}")))?;
        Ok(YahooQuote {
            symbol: symbol.to_string(),
            price,
            currency,
            at,
        })
    }

    fn chart_url(&self, symbol: &str, query: &[(&str, String)]) -> Result<Url, YahooError> {
        let mut url = Url::parse(&self.base_url)
            .map_err(|e| YahooError::Parse(format!("URL base inválida: {e}")))?;
        url.path_segments_mut()
            .map_err(|_| YahooError::Parse("URL base inválida".into()))?
            .pop_if_empty()
            .extend(["v8", "finance", "chart"])
            .push(symbol);
        url.query_pairs_mut().extend_pairs(query);
        Ok(url)
    }

    /// Fetches `chart.result[0]` with retries; maps API errors and empty results.
    async fn fetch_chart(
        &self,
        symbol: &str,
        query: &[(&str, String)],
    ) -> Result<ChartResult, YahooError> {
        let url = self.chart_url(symbol, query)?;
        let body = self.get_with_retry(url).await?;
        let parsed: ChartResponse =
            serde_json::from_slice(&body).map_err(|e| YahooError::Parse(e.to_string()))?;
        if let Some(err) = parsed.chart.error {
            return Err(YahooError::Api {
                code: err.code.unwrap_or_default(),
                description: err.description.unwrap_or_default(),
            });
        }
        parsed
            .chart
            .result
            .and_then(|r| r.into_iter().next())
            .ok_or_else(|| YahooError::NoData(symbol.to_string()))
    }

    async fn get_with_retry(&self, url: Url) -> Result<Vec<u8>, YahooError> {
        let mut attempt = 1;
        loop {
            let last = attempt >= self.max_attempts;
            match self
                .http
                .get(url.clone())
                .header(reqwest::header::USER_AGENT, USER_AGENT)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp.bytes().await?.to_vec());
                    }
                    let retryable =
                        status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                    if !retryable || last {
                        return Err(YahooError::Status(status.as_u16()));
                    }
                    tracing::warn!(%status, attempt, "Yahoo request failed, retrying");
                }
                Err(e) => {
                    if !(e.is_connect() || e.is_timeout()) || last {
                        return Err(e.into());
                    }
                    tracing::warn!(error = %e, attempt, "Yahoo request error, retrying");
                }
            }
            tokio::time::sleep(self.retry_delay).await;
            attempt += 1;
        }
    }
}
