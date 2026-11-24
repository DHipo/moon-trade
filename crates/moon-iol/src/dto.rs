use chrono::{DateTime, NaiveDate, Utc};
use moon_core::asset::Market;
use moon_core::bars::Bar;
use moon_core::time::art_date;
use moon_core::types::{OrderType, Side, Term};
use serde::{Deserialize, Deserializer, Serialize};

/// Accepts a JSON string or an integer and yields a string (IOL ids come in both shapes).
pub fn string_or_number<'de, D>(d: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Text(String),
        Int(i64),
        Uint(u64),
        Float(f64),
    }
    match Id::deserialize(d)? {
        Id::Text(s) => Ok(s),
        Id::Int(i) => Ok(i.to_string()),
        Id::Uint(u) => Ok(u.to_string()),
        Id::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15 => {
            Ok(format!("{f:.0}"))
        }
        Id::Float(f) => Err(serde::de::Error::custom(format!("id inválido: {f}"))),
    }
}

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
    #[serde(deserialize_with = "string_or_number")]
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

/// One response of the operations list. IOL documents no pagination, only
/// `meta.total_count`, so a page may be truncated when `total_count > items.len()`.
#[derive(Debug, Clone)]
pub struct OperationsPage {
    pub items: Vec<IolOperation>,
    pub total_count: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolOpAmounts {
    pub qty: Option<f64>,
    pub price: Option<f64>,
    pub amount: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IolOperation {
    #[serde(deserialize_with = "string_or_number")]
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
    #[serde(deserialize_with = "string_or_number")]
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
    #[serde(deserialize_with = "string_or_number")]
    pub order_id: String,
    pub status: Option<String>,
    #[serde(default)]
    pub cancelled: bool,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_accept_number_or_string() {
        let a: IolOrderAck =
            serde_json::from_value(json!({"order_id": 12345, "successful_purchase": true}))
                .unwrap();
        assert_eq!(a.order_id, "12345");
        let a: IolOrderAck =
            serde_json::from_value(json!({"order_id": "ab-1", "successful_purchase": true}))
                .unwrap();
        assert_eq!(a.order_id, "ab-1");
        let o: IolOperation =
            serde_json::from_value(json!({"order_id": 77, "status": "pending"})).unwrap();
        assert_eq!(o.order_id, "77");
        let act: IolActivity = serde_json::from_value(json!({"id": 9001})).unwrap();
        assert_eq!(act.id, "9001");
        let act: IolActivity = serde_json::from_value(json!({"id": "x9"})).unwrap();
        assert_eq!(act.id, "x9");
        let c: IolCancel =
            serde_json::from_value(json!({"order_id": 5, "cancelled": true})).unwrap();
        assert_eq!(c.order_id, "5");
        assert!(serde_json::from_value::<IolActivity>(json!({"id": null})).is_err());
        assert!(serde_json::from_value::<IolActivity>(json!({"id": 1.5})).is_err());
    }
}
