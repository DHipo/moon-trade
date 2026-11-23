//! Persisted record types.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use moon_core::asset::{AssetId, Currency, Market};
use moon_core::types::{ExecMode, OrderReason, OrderStatus, Side};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRecord {
    pub id: Uuid,
    pub batch_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub mode: ExecMode,
    pub reason: OrderReason,
    pub asset: AssetId,
    pub symbol: String,
    pub market: Market,
    pub side: Side,
    pub qty: Decimal,
    pub limit_price: Option<Decimal>,
    pub currency: Currency,
    pub status: OrderStatus,
    pub broker_order_id: Option<String>,
    pub filled_qty: Decimal,
    pub avg_fill_price: Option<Decimal>,
    pub fees: Decimal,
    pub fees_estimated: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub at: DateTime<Utc>,
    pub mode: ExecMode,
    pub cash_ars: Decimal,
    pub equity_ars: Decimal,
    pub ccl: Decimal,
    pub equity_usd: Decimal,
    pub nav: f64,
    pub units: Decimal,
    pub drawdown: f64,
    pub positions: BTreeMap<String, Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxRecord {
    pub at: DateTime<Utc>,
    pub rate: Decimal,
    pub method: String,
    pub components: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Monthly,
    Deposit,
}

impl ProposalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Monthly => "monthly",
            Self::Deposit => "deposit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
    Executed,
    Failed,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
            Self::Executed => "executed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub kind: ProposalKind,
    pub status: ProposalStatus,
    pub decided_at: Option<DateTime<Utc>>,
    pub payload: serde_json::Value,
    pub telegram_message_id: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PaperAccount {
    pub cash_ars: Decimal,
    pub positions: BTreeMap<String, Decimal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    Deposit,
    Withdrawal,
}

impl FlowKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Deposit => "deposit",
            Self::Withdrawal => "withdrawal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowRecord {
    pub at: DateTime<Utc>,
    pub mode: ExecMode,
    pub kind: FlowKind,
    pub amount_ars: Decimal,
    pub external_id: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestRun {
    pub id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub config_hash: String,
    pub config: serde_json::Value,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub include_holdout: bool,
    pub metrics: serde_json::Value,
}
