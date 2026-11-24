//! Execution layer for moon-trade: paper and IOL brokers, quote sources and implied CCL.
//!
//! IOL has no sandbox, so paper mode simulates fills locally.

mod fx;
mod iol_broker;
mod paper;
mod quotes;

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use moon_core::asset::{Currency, Market};
use moon_core::types::{ExecMode, OrderStatus, OrderType, Side, Term};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use fx::{FxComponent, FxPair, FxQuote, implied_ccl};
pub use iol_broker::IolBroker;
pub use paper::PaperBroker;
pub use quotes::{
    IolQuotes, Quote, QuoteSource, StaticQuotes, YahooQuotes, is_fresh_at, parse_currency,
    parse_quote_time,
};

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("cotización: {0}")]
    Quote(String),
    #[error("broker: {0}")]
    Broker(String),
    #[error("moneda inesperada para {symbol}: se esperaba {expected:?}, llegó {got}")]
    CurrencyMismatch {
        symbol: String,
        expected: Currency,
        got: String,
    },
    #[error("no encontrado: {0}")]
    NotFound(String),
    #[error("base de datos: {0}")]
    Store(#[from] moon_store::StoreError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountState {
    /// Cash available to buy with (excludes amounts committed to open orders).
    pub cash_ars: Decimal,
    /// Total cash owned, including amounts committed to open orders; use it for equity.
    pub cash_total_ars: Decimal,
    pub positions: BTreeMap<String, Decimal>,
    pub sellable: BTreeMap<String, Decimal>,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderTicket {
    pub idempotency_key: Uuid,
    pub symbol: String,
    pub market: Market,
    pub side: Side,
    pub qty: Decimal,
    pub order_type: OrderType,
    pub limit_price: Option<Decimal>,
    pub due_date: NaiveDate,
    pub settlement_term: Term,
}

/// Whether the broker's order list contained the order that was refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Listing {
    /// The order was found (or the broker has no such notion, e.g. paper).
    #[default]
    Listed,
    /// The list did not include the order.
    NotListed,
    /// The list did not include the order and the broker's list was truncated.
    NotListedTruncated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderProgress {
    pub status: OrderStatus,
    pub filled_qty: Decimal,
    pub avg_price: Option<Decimal>,
    pub fees: Option<Decimal>,
    #[serde(default)]
    pub listing: Listing,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaceResult {
    pub broker_order_id: String,
    pub accepted: bool,
    pub messages: Vec<String>,
    pub fill: Option<OrderProgress>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExternalFlow {
    pub external_id: String,
    pub kind: moon_store::FlowKind,
    pub amount_ars: Decimal,
    pub at: DateTime<Utc>,
}

#[async_trait::async_trait]
pub trait Broker: Send + Sync {
    fn mode(&self) -> ExecMode;
    async fn account(&self) -> Result<AccountState, BrokerError>;
    async fn place(&self, ticket: &OrderTicket) -> Result<PlaceResult, BrokerError>;
    /// Current state of an order. `symbol` is the symbol the order was placed with (persisted by
    /// the caller, so it survives restarts) and narrows the broker's operations query.
    async fn refresh(
        &self,
        broker_order_id: &str,
        symbol: &str,
        placed_on: NaiveDate,
    ) -> Result<OrderProgress, BrokerError>;
    /// Requests the cancellation. `Ok` only when the broker CONFIRMS it; callers still `refresh`
    /// before acting on the outcome.
    async fn cancel(&self, broker_order_id: &str) -> Result<(), BrokerError>;
    async fn external_flows(
        &self,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<ExternalFlow>, BrokerError>;
}
