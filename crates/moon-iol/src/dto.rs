use chrono::{DateTime, NaiveDate, Utc};
use moon_core::asset::Market;
use moon_core::bars::Bar;
use moon_core::time::art_date;
use moon_core::types::{OrderType, Side, Term};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct IolBar {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    #[serde(default)]
    pub volume: f64,
    pub timestamp: i64,
}

impl IolBar {
    /// Converts to a core bar; the date is the timestamp in Argentina time, `adj_close = close`.
    pub fn to_core_bar(&self) -> Bar {
        let t = DateTime::<Utc>::from_timestamp(self.timestamp, 0).unwrap_or(DateTime::UNIX_EPOCH);
        Bar {
            date: art_date(t),
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            adj_close: self.close,
            volume: self.volume,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IolPrices {
    pub current: Option<f64>,
    pub open: Option<f64>,
    pub close: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub previous_close: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolQuote {
    pub symbol: String,
    pub market: Option<String>,
    pub settlement_term: Option<String>,
    pub currency: Option<String>,
    #[serde(default)]
    pub prices: IolPrices,
    pub lot_size: Option<f64>,
    pub last_updated: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TermAmounts {
    pub t0: Option<f64>,
    pub t1: Option<f64>,
    pub t2: Option<f64>,
    pub t3: Option<f64>,
}

impl TermAmounts {
    pub fn get(&self, term: Term) -> Option<f64> {
        match term {
            Term::T0 => self.t0,
            Term::T1 => self.t1,
            Term::T2 => self.t2,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IolCurrencyBalance {
    pub currency: Option<String>,
    #[serde(default)]
    pub totals: TermAmounts,
    #[serde(default)]
    pub available: TermAmounts,
    #[serde(default)]
    pub committed: TermAmounts,
    pub available_for_withdrawal: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IolBalances {
    pub ars: Option<IolCurrencyBalance>,
    pub usd: Option<IolCurrencyBalance>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolInstrument {
    pub symbol: String,
    #[serde(rename = "type")]
    pub instrument_type: Option<String>,
    pub currency: Option<String>,
    pub exchange: Option<String>,
    pub lot_size: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolAvailability {
    pub quantity_available: Option<f64>,
    pub quantity_held: Option<f64>,
    pub quantity_pending: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolMarketValue {
    pub current_price: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolPosition {
    pub quantity: f64,
    pub instrument: IolInstrument,
    pub availability: Option<IolAvailability>,
    pub market_value: Option<IolMarketValue>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IolPositions {
    #[serde(default)]
    pub positions: Vec<IolPosition>,
    pub total_market_value: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IolOrderRequest {
    pub symbol: String,
    pub market_id: Market,
    pub side: Side,
    pub order_type: OrderType,
    /// Serialized as `YYYY-MM-DD`.
    pub due_date: NaiveDate,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<f64>,
    pub quantity: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settlement_term: Option<Term>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolMessage {
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolOrderAck {
    pub order_id: String,
    pub successful_purchase: bool,
    #[serde(default)]
    pub messages: Vec<IolMessage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolMoney {
    pub currency: Option<String>,
    pub value: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolRegulatoryFees {
    pub market_fee: Option<IolMoney>,
    pub vat: Option<IolMoney>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolEstimate {
    pub amount: Option<IolMoney>,
    pub commission: Option<IolMoney>,
    pub regulatory_fees: Option<IolRegulatoryFees>,
}

#[derive(Debug, Clone, Default)]
pub struct OperationsQuery {
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub country: Option<String>,
    pub symbol: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolOpAmounts {
    pub qty: Option<f64>,
    pub price: Option<f64>,
    pub amount: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolOperation {
    pub order_id: String,
    pub status: String,
    pub symbol: Option<String>,
    pub order_date: Option<String>,
    pub currency: Option<String>,
    pub order: Option<IolOpAmounts>,
    pub executed: Option<IolOpAmounts>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolFees {
    pub commission: Option<f64>,
    pub vat: Option<f64>,
    pub other: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolActivity {
    pub id: String,
    pub activity_type: Option<String>,
    pub trade_date: Option<String>,
    pub net_amount: Option<f64>,
    pub currency: Option<String>,
    pub status: Option<String>,
    pub fees: Option<IolFees>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolCancel {
    pub order_id: String,
    pub status: Option<String>,
    #[serde(default)]
    pub cancelled: bool,
}
