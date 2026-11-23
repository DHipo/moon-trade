use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use moon_core::asset::{Currency, Market};
use moon_core::money::dec_from_f64;
use moon_core::types::Term;
use moon_iol::IolClient;
use moon_yahoo::YahooClient;
use rust_decimal::Decimal;

use crate::BrokerError;

#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub symbol: String,
    pub market: Market,
    pub price: Decimal,
    pub currency: Currency,
    pub at: DateTime<Utc>,
}

#[async_trait::async_trait]
pub trait QuoteSource: Send + Sync {
    async fn quote(&self, symbol: &str, market: Market) -> Result<Quote, BrokerError>;
}

/// Parses a currency label as reported by IOL or Yahoo (case-insensitive).
pub fn parse_currency(raw: &str) -> Option<Currency> {
    let up = raw.to_uppercase();
    if up.contains("ARS") || up.contains("PESO") {
        Some(Currency::Ars)
    } else if up.contains("USD") || up.contains("DOLAR") || up.contains("DÓLAR") {
        Some(Currency::Usd)
    } else {
        None
    }
}

pub struct IolQuotes {
    client: Arc<IolClient>,
    term: Term,
}

impl IolQuotes {
    pub fn new(client: Arc<IolClient>, term: Term) -> Self {
        Self { client, term }
    }
}

#[async_trait::async_trait]
impl QuoteSource for IolQuotes {
    async fn quote(&self, symbol: &str, market: Market) -> Result<Quote, BrokerError> {
        let q = self
            .client
            .quote(symbol, market, self.term)
            .await
            .map_err(|e| BrokerError::Quote(format!("{symbol}: {e}")))?;
        let p = &q.prices;
        let raw = [p.current, p.close, p.previous_close]
            .into_iter()
            .flatten()
            .find(|v| *v > 0.0)
            .ok_or_else(|| BrokerError::Quote(format!("{symbol}: sin precio")))?;
        let price = dec_from_f64(raw)
            .ok_or_else(|| BrokerError::Quote(format!("{symbol}: precio inválido")))?;
        let raw_ccy = q.currency.clone().unwrap_or_default();
        let currency = parse_currency(&raw_ccy).ok_or_else(|| BrokerError::CurrencyMismatch {
            symbol: symbol.to_string(),
            expected: Currency::Ars,
            got: raw_ccy,
        })?;
        Ok(Quote {
            symbol: symbol.to_string(),
            market,
            price,
            currency,
            at: Utc::now(),
        })
    }
}

pub struct YahooQuotes {
    client: YahooClient,
}

impl YahooQuotes {
    pub fn new(client: YahooClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl QuoteSource for YahooQuotes {
    async fn quote(&self, symbol: &str, market: Market) -> Result<Quote, BrokerError> {
        let ysym = match market {
            Market::Bcba => format!("{symbol}.BA"),
            Market::Nyse | Market::Nasdaq => symbol.to_string(),
        };
        let q = self
            .client
            .latest_quote(&ysym)
            .await
            .map_err(|e| BrokerError::Quote(format!("{symbol}: {e}")))?;
        let price = dec_from_f64(q.price)
            .filter(|p| *p > Decimal::ZERO)
            .ok_or_else(|| BrokerError::Quote(format!("{symbol}: precio inválido")))?;
        let currency =
            parse_currency(&q.currency).ok_or_else(|| BrokerError::CurrencyMismatch {
                symbol: symbol.to_string(),
                expected: Currency::Ars,
                got: q.currency.clone(),
            })?;
        Ok(Quote {
            symbol: symbol.to_string(),
            market,
            price,
            currency,
            at: q.at,
        })
    }
}

/// In-memory quote source for tests.
#[derive(Default)]
pub struct StaticQuotes {
    quotes: Mutex<BTreeMap<(String, Market), Quote>>,
}

impl StaticQuotes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, symbol: &str, market: Market, price: Decimal, currency: Currency) {
        let q = Quote {
            symbol: symbol.to_string(),
            market,
            price,
            currency,
            at: Utc::now(),
        };
        if let Ok(mut m) = self.quotes.lock() {
            m.insert((symbol.to_string(), market), q);
        }
    }

    pub fn remove(&self, symbol: &str, market: Market) {
        if let Ok(mut m) = self.quotes.lock() {
            m.remove(&(symbol.to_string(), market));
        }
    }
}

#[async_trait::async_trait]
impl QuoteSource for StaticQuotes {
    async fn quote(&self, symbol: &str, market: Market) -> Result<Quote, BrokerError> {
        let m = self
            .quotes
            .lock()
            .map_err(|_| BrokerError::Quote("estado interno envenenado".into()))?;
        m.get(&(symbol.to_string(), market))
            .cloned()
            .ok_or_else(|| BrokerError::Quote(format!("{symbol}: sin cotización")))
    }
}
