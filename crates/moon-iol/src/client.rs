use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDate;
use moon_core::asset::Market;
use moon_core::time::Clock;
use moon_core::types::Term;
use reqwest::{Method, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::auth::{TokenData, TokenState};
use crate::dto::{
    IolActivity, IolBalances, IolBar, IolCancel, IolEstimate, IolOperation, IolOrderAck,
    IolOrderRequest, IolPositions, IolQuote, OperationsQuery,
};
use crate::error::{IolCredentials, IolError};

pub const DEFAULT_BASE_URL: &str = "https://api.iolinversiones.com";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Account country used in IOL account routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Country {
    Ar,
    Us,
}

impl Country {
    pub fn as_str(&self) -> &'static str {
        match self {
            Country::Ar => "AR",
            Country::Us => "US",
        }
    }
}

pub struct IolClient {
    http: reqwest::Client,
    base_url: String,
    creds: IolCredentials,
    clock: Arc<dyn Clock>,
    token: Mutex<Option<TokenState>>,
}

impl fmt::Debug for IolClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IolClient")
            .field("base_url", &self.base_url)
            .field("username", &self.creds.username)
            .field("credentials", &"***")
            .field("token", &"***")
            .finish()
    }
}

fn build_http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not build HTTP client with timeouts, using default");
            reqwest::Client::new()
        })
}

#[derive(Deserialize)]
struct BarsData {
    #[serde(default)]
    bars: Vec<IolBar>,
}

fn status_text(status: reqwest::StatusCode) -> String {
    status
        .canonical_reason()
        .map_or_else(|| status.to_string(), str::to_string)
}

impl IolClient {
    pub fn new(base_url: impl Into<String>, creds: IolCredentials, clock: Arc<dyn Clock>) -> Self {
        let base_url: String = base_url.into();
        let base_url = if base_url.trim().is_empty() {
            DEFAULT_BASE_URL.to_string()
        } else {
            base_url
        };
        Self {
            http: build_http(),
            base_url,
            creds,
            clock,
            token: Mutex::new(None),
        }
    }

    pub async fn bars(
        &self,
        symbol: &str,
        market: Market,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<IolBar>, IolError> {
        let url = self.url(
            &["broker", "market-data", symbol, "bars"],
            &[
                ("market", market.as_str().to_string()),
                ("from", format!("{} 00:00:00", from.format("%Y-%m-%d"))),
                ("to", format!("{} 23:59:59", to.format("%Y-%m-%d"))),
            ],
        )?;
        let data: BarsData = self.call(Method::GET, url, None, None).await?;
        Ok(data.bars)
    }

    pub async fn quote(
        &self,
        symbol: &str,
        market: Market,
        term: Term,
    ) -> Result<IolQuote, IolError> {
        let url = self.url(
            &["broker", "market-data", symbol, "quote"],
            &[
                ("market", market.as_str().to_string()),
                ("term", term.as_str().to_string()),
            ],
        )?;
        self.call(Method::GET, url, None, None).await
    }

    pub async fn balances(&self, country: Country) -> Result<IolBalances, IolError> {
        let url = self.url(&["broker", "accounts", country.as_str(), "balances"], &[])?;
        self.call(Method::GET, url, None, None).await
    }

    pub async fn positions(&self, country: Country) -> Result<IolPositions, IolError> {
        let url = self.url(&["broker", "accounts", country.as_str(), "positions"], &[])?;
        self.call(Method::GET, url, None, None).await
    }

    /// Places a REAL order. The idempotency key lets the caller retry safely.
    pub async fn place_order(
        &self,
        req: &IolOrderRequest,
        idempotency_key: Uuid,
    ) -> Result<IolOrderAck, IolError> {
        let url = self.url(&["broker", "orders"], &[])?;
        let body = serde_json::to_value(req).map_err(|e| IolError::Parse(e.to_string()))?;
        self.call(Method::POST, url, Some(body), Some(idempotency_key))
            .await
    }

    pub async fn estimate_order(&self, req: &IolOrderRequest) -> Result<IolEstimate, IolError> {
        let url = self.url(&["broker", "orders", "estimate"], &[])?;
        let body = serde_json::to_value(req).map_err(|e| IolError::Parse(e.to_string()))?;
        self.call(Method::POST, url, Some(body), None).await
    }

    pub async fn operations(&self, q: &OperationsQuery) -> Result<Vec<IolOperation>, IolError> {
        let mut query = vec![
            ("start_date", q.start_date.format("%Y-%m-%d").to_string()),
            ("end_date", q.end_date.format("%Y-%m-%d").to_string()),
        ];
        if let Some(c) = &q.country {
            query.push(("country", c.clone()));
        }
        if let Some(s) = &q.symbol {
            query.push(("symbol", s.clone()));
        }
        if let Some(s) = &q.state {
            query.push(("state", s.clone()));
        }
        let url = self.url(&["broker", "accounts", "activities", "operations"], &query)?;
        self.call(Method::GET, url, None, None).await
    }

    pub async fn activities(
        &self,
        start: NaiveDate,
        end: NaiveDate,
        movement_type: Option<&str>,
    ) -> Result<Vec<IolActivity>, IolError> {
        let mut query = vec![
            ("start_date", start.format("%Y-%m-%d").to_string()),
            ("end_date", end.format("%Y-%m-%d").to_string()),
        ];
        if let Some(m) = movement_type {
            query.push(("movement_type", m.to_string()));
        }
        let url = self.url(&["broker", "accounts", "activities"], &query)?;
        self.call(Method::GET, url, None, None).await
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<IolCancel, IolError> {
        let url = self.url(&["broker", "orders", order_id], &[])?;
        self.call(Method::DELETE, url, None, None).await
    }

    fn url(&self, segments: &[&str], query: &[(&str, String)]) -> Result<Url, IolError> {
        let mut url = Url::parse(&self.base_url)
            .map_err(|_| IolError::Parse("URL base de IOL inválida".to_string()))?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| IolError::Parse("URL base de IOL inválida".to_string()))?;
            path.pop_if_empty();
            for seg in segments {
                if seg.is_empty() || *seg == "." || *seg == ".." {
                    return Err(IolError::Parse(format!(
                        "segmento de ruta inválido: '{seg}'"
                    )));
                }
                path.push(seg);
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        Ok(url)
    }

    /// Authenticated call with a single retry after a 401 (token cleared, fresh login).
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        idempotency_key: Option<Uuid>,
    ) -> Result<T, IolError> {
        let mut retried = false;
        loop {
            let access = self.access_token().await?;
            let mut rb = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(&access);
            if let Some(b) = &body {
                rb = rb.json(b);
            }
            if let Some(k) = idempotency_key {
                rb = rb.header("X-Idempotency-Key", k.to_string());
            }
            match self.execute(rb).await {
                Err(IolError::Api { status: 401, .. }) if !retried => {
                    retried = true;
                    tracing::debug!(
                        path = url.path(),
                        "401 from IOL, clearing token and retrying"
                    );
                    *self.token.lock().await = None;
                }
                Ok(data) => {
                    return serde_json::from_value(data)
                        .map_err(|e| IolError::Parse(e.to_string()));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Sends the request and unwraps the `{meta, data, error}` envelope into `data`.
    async fn execute(&self, rb: reqwest::RequestBuilder) -> Result<Value, IolError> {
        let resp = rb.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        let mut v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                return Err(if status.is_success() {
                    IolError::Parse(e.to_string())
                } else {
                    IolError::Api {
                        status: status.as_u16(),
                        code: None,
                        message: status_text(status),
                    }
                });
            }
        };
        let err = v.get("error").filter(|e| !e.is_null());
        if !status.is_success() || err.is_some() {
            let message = err
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map_or_else(|| status_text(status), str::to_string);
            let code = err.and_then(|e| e.get("code")).and_then(|c| match c {
                Value::String(s) => Some(s.clone()),
                Value::Null => None,
                other => Some(other.to_string()),
            });
            return Err(IolError::Api {
                status: status.as_u16(),
                code,
                message,
            });
        }
        Ok(v.get_mut("data").map(Value::take).unwrap_or(Value::Null))
    }

    /// Returns a usable access token: cached, refreshed, or from a fresh login.
    /// The lock is held across the network calls so concurrent callers log in once.
    async fn access_token(&self) -> Result<String, IolError> {
        let mut guard = self.token.lock().await;
        let now = self.clock.now();
        if let Some(t) = guard.as_ref()
            && t.is_fresh(now)
        {
            return Ok(t.access_token.clone());
        }
        let refresh = guard.as_ref().and_then(|t| t.refresh_token.clone());
        if let Some(rt) = refresh {
            let body = json!({"grant_type": "refresh_token", "refresh_token": rt});
            match self.token_request(body).await {
                Ok(state) => {
                    let access = state.access_token.clone();
                    *guard = Some(state);
                    return Ok(access);
                }
                Err(e) => {
                    tracing::debug!(error = %e, "token refresh failed, logging in again");
                }
            }
        }
        *guard = None;
        let body = json!({
            "grant_type": "password",
            "username": self.creds.username,
            "password": self.creds.password,
        });
        let state = self.token_request(body).await.map_err(|e| match e {
            IolError::Api { message, .. } => IolError::Auth(message),
            other => other,
        })?;
        let access = state.access_token.clone();
        *guard = Some(state);
        Ok(access)
    }

    /// POST /auth/token. The body (credentials) is never logged.
    async fn token_request(&self, body: Value) -> Result<TokenState, IolError> {
        let url = self.url(&["auth", "token"], &[])?;
        let data = self.execute(self.http.post(url).json(&body)).await?;
        let token: TokenData =
            serde_json::from_value(data).map_err(|e| IolError::Parse(e.to_string()))?;
        Ok(token.into_state(self.clock.now()))
    }
}
