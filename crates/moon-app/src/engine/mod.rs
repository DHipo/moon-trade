//! Execution engine: valuation, planning, order execution, approvals, risk checks and daily close.
//!
//! Safety rules enforced here (IOL has no sandbox, every live order is real):
//! * the store mutex guard is never held across an `.await`;
//! * trading operations are serialized with the engine's trading lock;
//! * an order whose outcome is unknown is never re-sent with a new idempotency key.

mod execution;
mod jobs;
pub mod reconcile;
mod text;
mod valuation;

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use moon_broker::{Broker, QuoteSource};
use moon_core::allocator::{AllocationDecision, Allocator};
use moon_core::asset::{AssetId, Universe};
use moon_core::nav::NavTracker;
use moon_core::risk::DrawdownState;
use moon_core::risk_index::RiskIndex;
use moon_core::strategy::SignalContext;
use moon_core::time::{ART, Clock, art_date};
use moon_core::types::{ExecMode, OrderReason, OrderStatus};
use moon_store::{OrderRecord, Store, StoreError};
use moon_yahoo::YahooClient;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{AppConfig, TradingMode};
use crate::notify::Notifier;

pub use crate::scheduler::previous_trading_day;
pub use jobs::monthly_batch_id;
pub use reconcile::{Discrepancy, order_id_for, order_id_for_tranche, reconcile_positions};
pub(crate) use text::status_es;

// Global keys.
pub const STATE_PAUSED: &str = "paused";
pub const STATE_TELEGRAM_OFFSET: &str = "telegram_offset";
/// Set once the paper account has been seeded with the configured initial cash.
pub const STATE_PAPER_INIT: &str = "paper_initialized";

// Mode-specific keys: always read and written through [`Engine::state_key`], which appends
// `:<mode>` so paper and live never share NAV, breaker, schedule or flow state.
/// NAV tracker (performance reporting only).
pub const STATE_NAV: &str = "nav";
/// Risk index the drawdown breaker works on.
pub const STATE_RISK_INDEX: &str = "risk_index";
pub const STATE_DRAWDOWN: &str = "drawdown";
pub const STATE_SCHEDULE: &str = "schedule";
/// Last day (ART) up to which external flows were queried.
pub const STATE_FLOWS_CHECKED: &str = "flows_checked_until";
/// Month and attempt counter of the monthly rebalance (the attempt is part of the batch id).
pub const STATE_MONTHLY_ATTEMPT: &str = "monthly_attempt";
/// Deposits detected but not invested yet (their external ids).
pub const STATE_DEPOSIT_PENDING: &str = "deposit_pending";
/// Trip date for which the refuge buy after an emergency exit was already attempted.
pub const STATE_EMERGENCY_REFUGE: &str = "emergency_refuge_done";
/// Per asset, the last day the user was warned about an emergency order with unknown outcome.
pub const STATE_EMERGENCY_UNKNOWN: &str = "emergency_unknown_notified";

/// Start of the error raised when the order limits block an execution.
pub(crate) const ORDER_BLOCKED: &str = "Orden bloqueada por los límites de riesgo";

/// Prefix of the error recorded when `place` failed and the order may exist at the broker.
pub const UNKNOWN_OUTCOME: &str = "resultado desconocido: verificar en IOL —";

pub struct EngineDeps {
    pub store: Arc<Mutex<Store>>,
    pub broker: Arc<dyn Broker>,
    pub quotes: Arc<dyn QuoteSource>,
    pub notifier: Arc<dyn Notifier>,
    pub clock: Arc<dyn Clock>,
}

pub struct Engine {
    cfg: AppConfig,
    universe: Universe,
    allocator: Allocator,
    deps: EngineDeps,
    /// Yahoo client for the daily data sync; `None` skips the sync (tests).
    yahoo: Option<YahooClient>,
    trading_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionValue {
    pub asset: AssetId,
    pub symbol: String,
    pub qty: Decimal,
    pub sellable: Decimal,
    pub price_ars: Decimal,
    pub value_ars: Decimal,
}

/// Portfolio valuation. `positions` has one entry per tradable asset (quantity may be zero) so
/// that every tradable asset carries its current price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Valuation {
    pub at: DateTime<Utc>,
    /// Cash available for buying.
    pub cash_ars: Decimal,
    /// Cash including committed funds (pending buys, settlement): used for equity.
    pub cash_total_ars: Decimal,
    pub positions: Vec<PositionValue>,
    pub untracked: Vec<(String, Decimal)>,
    /// Tradable assets that are not held and had no usable quote: they cannot be bought now.
    pub unpriced: Vec<AssetId>,
    pub equity_ars: Decimal,
    pub ccl: Decimal,
    pub equity_usd: Decimal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionReport {
    pub batch_id: Uuid,
    pub orders: Vec<OrderRecord>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MonthlyOutcome {
    Executed(ExecutionReport),
    ProposalSent(Uuid),
    NoChanges,
    /// The monthly could not be carried out (stale signal data); it stays due.
    Aborted(String),
}

impl MonthlyOutcome {
    /// The month counts as done only if the plan was empty, a proposal was sent or at least one
    /// order was accepted by the broker; otherwise the scheduler retries after a backoff.
    pub fn is_complete(&self) -> bool {
        match self {
            Self::NoChanges | Self::ProposalSent(_) => true,
            Self::Executed(r) => r
                .orders
                .iter()
                .any(|o| !matches!(o.status, OrderStatus::Rejected | OrderStatus::Failed)),
            Self::Aborted(_) => false,
        }
    }
}

/// Stored per mode: which month the attempt counter belongs to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MonthlyAttempt {
    pub month: String,
    pub attempt: u32,
}

/// Last calendar day of the month before `today`'s month.
pub fn last_day_of_previous_month(today: NaiveDate) -> NaiveDate {
    today.with_day(1).unwrap_or(today) - Duration::days(1)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RiskOutcome {
    /// Risk index level (chained market return of the invested part, in USD).
    pub level: f64,
    pub drawdown: f64,
    pub tripped: bool,
    pub emergency: Option<ExecutionReport>,
}

/// Start and end (inclusive) of an ART calendar day, in UTC.
pub(crate) fn art_day_bounds(date: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let naive = date.and_time(chrono::NaiveTime::MIN);
    let start = ART
        .from_local_datetime(&naive)
        .earliest()
        .map_or_else(|| Utc.from_utc_datetime(&naive), |d| d.with_timezone(&Utc));
    (start, start + Duration::days(1) - Duration::seconds(1))
}

impl Engine {
    pub fn new(cfg: AppConfig, deps: EngineDeps) -> anyhow::Result<Engine> {
        cfg.validate()?;
        match cfg.mode {
            TradingMode::Paper => {
                if deps.broker.mode() != ExecMode::Paper {
                    bail!("El modo paper requiere un broker simulado.");
                }
            }
            TradingMode::LiveConfirm | TradingMode::LiveAuto => {
                if !cfg.live.enabled {
                    bail!("El modo live requiere [live] enabled = true (doble confirmación).");
                }
                if deps.broker.mode() != ExecMode::Live {
                    bail!("El modo live requiere el broker real de IOL.");
                }
            }
        }
        let universe = cfg.universe()?;
        let allocator = cfg.allocator()?;
        Ok(Engine {
            cfg,
            universe,
            allocator,
            deps,
            yahoo: None,
            trading_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Enables the Yahoo data sync of the daily close.
    pub fn with_data_sync(mut self, yahoo: YahooClient) -> Self {
        self.yahoo = Some(yahoo);
        self
    }

    pub fn config(&self) -> &AppConfig {
        &self.cfg
    }

    pub fn deps(&self) -> &EngineDeps {
        &self.deps
    }

    pub fn exec_mode(&self) -> ExecMode {
        match self.cfg.mode {
            TradingMode::Paper => ExecMode::Paper,
            TradingMode::LiveConfirm | TradingMode::LiveAuto => ExecMode::Live,
        }
    }

    pub fn set_paused(&self, paused: bool) -> anyhow::Result<()> {
        self.db(|s| s.set_state(STATE_PAUSED, &paused))
    }

    pub fn is_paused(&self) -> anyhow::Result<bool> {
        Ok(self
            .db(|s| s.get_state::<bool>(STATE_PAUSED))?
            .unwrap_or(false))
    }

    pub fn decide(&self, as_of: NaiveDate) -> anyhow::Result<AllocationDecision> {
        let series = {
            let store = self
                .deps
                .store
                .lock()
                .map_err(|_| anyhow!("la base de datos quedó en un estado inconsistente"))?;
            crate::commands::data::load_series(&self.cfg, &store)?
        };
        let ctx = SignalContext::new(as_of, &self.universe, &series);
        Ok(self.allocator.allocate(&ctx))
    }

    // --- internal helpers ---

    /// Runs `f` with the store locked. The guard never outlives the closure, so it can never be
    /// held across an `.await`.
    pub(crate) fn db<R>(
        &self,
        f: impl FnOnce(&mut Store) -> Result<R, StoreError>,
    ) -> anyhow::Result<R> {
        let mut guard = self
            .deps
            .store
            .lock()
            .map_err(|_| anyhow!("la base de datos quedó en un estado inconsistente"))?;
        Ok(f(&mut guard)?)
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        self.deps.clock.now()
    }

    pub(crate) fn today(&self) -> NaiveDate {
        art_date(self.now())
    }

    /// Key of a mode-specific state entry: `<base>:<mode>`.
    pub fn state_key(&self, base: &str) -> String {
        format!("{base}:{}", self.exec_mode().as_str())
    }

    /// Loads the NAV tracker, starting it at `initial_equity_usd` when absent or empty.
    pub(crate) fn load_tracker(&self, initial_equity_usd: Decimal) -> anyhow::Result<NavTracker> {
        let key = self.state_key(STATE_NAV);
        let existing = self.db(|s| s.get_state::<NavTracker>(&key))?;
        match existing {
            Some(t) if t.units > Decimal::ZERO => Ok(t),
            _ => {
                let t = NavTracker::start(initial_equity_usd);
                self.db(|s| s.set_state(&key, &t))?;
                Ok(t)
            }
        }
    }

    /// The risk index of this mode; a fresh one (level 1.0) when there is none yet.
    pub(crate) fn load_risk_index(&self) -> anyhow::Result<RiskIndex> {
        let key = self.state_key(STATE_RISK_INDEX);
        Ok(self
            .db(|s| s.get_state::<RiskIndex>(&key))?
            .unwrap_or_else(RiskIndex::new))
    }

    pub(crate) fn save_risk_index(&self, index: &RiskIndex) -> anyhow::Result<()> {
        let key = self.state_key(STATE_RISK_INDEX);
        self.db(|s| s.set_state(&key, index))
    }

    /// The breaker state of this mode; when absent, the peak starts at the current `level`.
    pub(crate) fn load_drawdown(&self, level: f64) -> anyhow::Result<DrawdownState> {
        let key = self.state_key(STATE_DRAWDOWN);
        let existing = self.db(|s| s.get_state::<DrawdownState>(&key))?;
        match existing {
            Some(d) => Ok(d),
            None => {
                let d = DrawdownState {
                    peak_nav: level,
                    ..DrawdownState::default()
                };
                self.db(|s| s.set_state(&key, &d))?;
                Ok(d)
            }
        }
    }

    pub(crate) fn save_drawdown(&self, d: &DrawdownState) -> anyhow::Result<()> {
        let key = self.state_key(STATE_DRAWDOWN);
        self.db(|s| s.set_state(&key, d))
    }

    /// NAV of `tracker` at `equity_usd` (performance reporting); an empty tracker is an explicit
    /// error, never a guess.
    pub(crate) fn nav_of(tracker: &NavTracker, equity_usd: Decimal) -> anyhow::Result<f64> {
        tracker.nav(equity_usd).ok_or_else(|| {
            anyhow!("no se puede calcular el NAV: no hay unidades (patrimonio en cero)")
        })
    }

    pub(crate) fn reason_label(reason: OrderReason) -> &'static str {
        match reason {
            OrderReason::Monthly => "rebalanceo mensual",
            OrderReason::Deposit => "depósito",
            OrderReason::Emergency => "salida de emergencia",
            OrderReason::Manual => "manual",
        }
    }
}
