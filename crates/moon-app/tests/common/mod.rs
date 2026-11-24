#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use moon_app::config::{AppConfig, TradingMode};
use moon_app::engine::{Engine, EngineDeps};
use moon_app::notify::RecordingNotifier;
use moon_broker::{
    AccountState, Broker, BrokerError, ExternalFlow, Listing, OrderProgress, OrderTicket,
    PaperBroker, PlaceResult, StaticQuotes,
};
use moon_core::allocator::{AllocationConfig, SizingKind, StrategyConfig};
use moon_core::asset::{Currency, Market, Role};
use moon_core::testutil::{d, daily_series, spec};
use moon_core::time::FixedClock;
use moon_core::types::{DataSource, ExecMode, OrderStatus};
use moon_store::{PaperAccount, Store};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

pub const TODAY: NaiveDate = match NaiveDate::from_ymd_opt(2026, 11, 23) {
    Some(d) => d,
    None => panic!("bad date"),
};

/// Monday 2026-11-23, 11:30 ART.
pub fn clock_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 23, 14, 30, 0)
        .single()
        .expect("valid time")
}

/// Configuration from `moon.example.toml` with two risky assets and CASH as refuge.
pub fn test_config(mode: TradingMode) -> AppConfig {
    let mut cfg = AppConfig::load(Path::new("../../moon.example.toml")).expect("example loads");
    cfg.mode = mode;
    cfg.live.enabled = mode != TradingMode::Paper;
    cfg.assets = vec![
        spec("AAA", Role::Risky),
        spec("BBB", Role::Risky),
        spec("CASH", Role::Refuge),
    ];
    cfg.allocation = AllocationConfig {
        strategies: vec![StrategyConfig::FaberSma { months: 6 }],
        sizing: SizingKind::Equal,
        vol_lookback_days: 63,
        canary_threshold: None,
    };
    cfg.risk.max_weight = 0.5;
    cfg.execution.poll_interval_seconds = 0;
    cfg.execution.fill_timeout_minutes = 0;
    // Tests that exercise the emergency wait raise this explicitly.
    cfg.execution.emergency_fill_wait_seconds = 0;
    cfg.validate().expect("test config valid");
    cfg
}

pub fn seed_series(store: &mut Store, cfg: &AppConfig) {
    for (i, sym) in cfg
        .universe()
        .expect("universe")
        .signal_symbols()
        .iter()
        .enumerate()
    {
        let closes: Vec<f64> = (0..1300)
            .map(|k| 100.0 + k as f64 * 0.05 + ((k as f64) * 0.07 + i as f64).sin() * 2.0)
            .collect();
        let series = daily_series(d(2022, 1, 3), &closes);
        store
            .upsert_bars(sym, DataSource::Yahoo, series.bars())
            .expect("upsert bars");
    }
}

/// Exec symbols at 10,000 ARS; FX pairs consistent with a CCL of exactly 1200.
pub fn seed_quotes(quotes: &StaticQuotes, cfg: &AppConfig) {
    for asset in cfg.universe().expect("universe").tradable() {
        let exec = asset.exec.as_ref().expect("exec");
        quotes.set(&exec.symbol, Market::Bcba, dec!(10000), Currency::Ars);
    }
    for p in &cfg.fx.pairs {
        quotes.set(&p.adr_symbol, p.adr_market, dec!(10), Currency::Usd);
        quotes.set(
            &p.local_symbol,
            Market::Bcba,
            dec!(1200) * dec!(10) / p.ratio,
            Currency::Ars,
        );
    }
}

/// Live-mode broker double: records every call and never fills by itself.
pub struct FakeBroker {
    places: Mutex<Vec<OrderTicket>>,
    cancels: Mutex<Vec<String>>,
    account: Mutex<AccountState>,
    refresh_result: Mutex<OrderProgress>,
    flows: Mutex<Vec<ExternalFlow>>,
    fail_place: Mutex<bool>,
    fail_buys: Mutex<bool>,
    reject_buys: Mutex<bool>,
    fail_flows: Mutex<bool>,
    reject_place: Mutex<bool>,
    /// `Some(fees)`: orders fill completely at placement and the account follows.
    fill_on_place: Mutex<Option<Option<Decimal>>>,
    /// `Some(n)`: an order fills completely on its n-th refresh and the account follows.
    fill_on_refresh: Mutex<Option<u32>>,
    refresh_counts: Mutex<BTreeMap<String, u32>>,
    /// `cancel` returns an error (IOL did not confirm).
    cancel_fails: Mutex<bool>,
    /// A successful `cancel` makes the order show as cancelled on the next refresh (default).
    cancel_confirms: Mutex<bool>,
    cancelled: Mutex<BTreeSet<String>>,
    /// Orders of this type are definitely rejected at placement.
    reject_type: Mutex<Option<moon_core::types::OrderType>>,
    /// `refresh` reports the order as missing from IOL's list.
    not_listed: Mutex<Option<Listing>>,
    fail_refresh: Mutex<bool>,
    /// Fills at placement only apply to sells.
    fill_only_sells: Mutex<bool>,
    /// Sell proceeds are held back until `release_pending_cash` (settlement lag).
    cash_lag: Mutex<bool>,
    pending_cash: Mutex<Decimal>,
    /// (broker order id, symbol) of every refresh call.
    refreshes: Mutex<Vec<(String, String)>>,
}

impl FakeBroker {
    pub fn new(cash_ars: Decimal) -> Self {
        Self {
            places: Mutex::new(Vec::new()),
            cancels: Mutex::new(Vec::new()),
            account: Mutex::new(AccountState {
                cash_ars,
                cash_total_ars: cash_ars,
                positions: BTreeMap::new(),
                sellable: BTreeMap::new(),
                as_of: clock_time(),
            }),
            refresh_result: Mutex::new(OrderProgress {
                status: OrderStatus::Sent,
                filled_qty: Decimal::ZERO,
                avg_price: None,
                fees: None,
                listing: Listing::Listed,
            }),
            flows: Mutex::new(Vec::new()),
            fail_place: Mutex::new(false),
            fail_buys: Mutex::new(false),
            reject_buys: Mutex::new(false),
            fail_flows: Mutex::new(false),
            reject_place: Mutex::new(false),
            fill_on_place: Mutex::new(None),
            fill_on_refresh: Mutex::new(None),
            refresh_counts: Mutex::new(BTreeMap::new()),
            cancel_fails: Mutex::new(false),
            cancel_confirms: Mutex::new(true),
            cancelled: Mutex::new(BTreeSet::new()),
            reject_type: Mutex::new(None),
            not_listed: Mutex::new(None),
            fail_refresh: Mutex::new(false),
            fill_only_sells: Mutex::new(false),
            cash_lag: Mutex::new(false),
            pending_cash: Mutex::new(Decimal::ZERO),
            refreshes: Mutex::new(Vec::new()),
        }
    }

    pub fn set_cancel_fails(&self, fails: bool) {
        *self.cancel_fails.lock().expect("lock") = fails;
    }

    /// `false`: `cancel` succeeds but the order keeps showing its previous state.
    pub fn set_cancel_confirms(&self, confirms: bool) {
        *self.cancel_confirms.lock().expect("lock") = confirms;
    }

    pub fn set_reject_type(&self, t: Option<moon_core::types::OrderType>) {
        *self.reject_type.lock().expect("lock") = t;
    }

    pub fn set_not_listed(&self, l: Option<Listing>) {
        *self.not_listed.lock().expect("lock") = l;
    }

    pub fn set_fill_only_sells(&self, only: bool) {
        *self.fill_only_sells.lock().expect("lock") = only;
    }

    pub fn set_cash_lag(&self, lag: bool) {
        *self.cash_lag.lock().expect("lock") = lag;
    }

    /// Credits the sell proceeds that were held back by the cash lag.
    pub fn release_pending_cash(&self) {
        let amount = std::mem::take(&mut *self.pending_cash.lock().expect("lock"));
        let mut a = self.account.lock().expect("lock");
        a.cash_ars += amount;
        a.cash_total_ars += amount;
    }

    pub fn set_fail_refresh(&self, fail: bool) {
        *self.fail_refresh.lock().expect("lock") = fail;
    }

    /// What every refresh reports (unless an order was cancelled or fills on refresh).
    pub fn set_refresh_result(&self, progress: OrderProgress) {
        *self.refresh_result.lock().expect("lock") = progress;
    }

    pub fn refreshes(&self) -> Vec<(String, String)> {
        self.refreshes.lock().expect("lock").clone()
    }

    pub fn set_position(&self, symbol: &str, qty: Decimal) {
        let mut a = self.account.lock().expect("lock");
        a.positions.insert(symbol.to_string(), qty);
        a.sellable.insert(symbol.to_string(), qty);
    }

    pub fn add_flow(&self, flow: ExternalFlow) {
        self.flows.lock().expect("lock").push(flow);
    }

    pub fn set_fail_place(&self, fail: bool) {
        *self.fail_place.lock().expect("lock") = fail;
    }

    pub fn set_fail_buys(&self, fail: bool) {
        *self.fail_buys.lock().expect("lock") = fail;
    }

    pub fn set_fail_flows(&self, fail: bool) {
        *self.fail_flows.lock().expect("lock") = fail;
    }

    pub fn set_reject_buys(&self, reject: bool) {
        *self.reject_buys.lock().expect("lock") = reject;
    }

    pub fn set_reject_place(&self, reject: bool) {
        *self.reject_place.lock().expect("lock") = reject;
    }

    pub fn set_fill_on_place(&self, fees: Option<Option<Decimal>>) {
        *self.fill_on_place.lock().expect("lock") = fees;
    }

    /// Orders fill completely on their `n`-th refresh (`None`: never).
    pub fn set_fill_on_refresh(&self, n: Option<u32>) {
        *self.fill_on_refresh.lock().expect("lock") = n;
    }

    /// Sets available cash and total cash (including committed funds) separately.
    pub fn set_cash(&self, available: Decimal, total: Decimal) {
        let mut a = self.account.lock().expect("lock");
        a.cash_ars = available;
        a.cash_total_ars = total;
    }

    pub fn set_sellable(&self, symbol: &str, qty: Decimal) {
        self.account
            .lock()
            .expect("lock")
            .sellable
            .insert(symbol.to_string(), qty);
    }

    pub fn account_snapshot(&self) -> AccountState {
        self.account.lock().expect("lock").clone()
    }

    pub fn places(&self) -> Vec<OrderTicket> {
        self.places.lock().expect("lock").clone()
    }

    pub fn cancels(&self) -> Vec<String> {
        self.cancels.lock().expect("lock").clone()
    }
}

#[async_trait::async_trait]
impl Broker for FakeBroker {
    fn mode(&self) -> ExecMode {
        ExecMode::Live
    }

    async fn account(&self) -> Result<AccountState, BrokerError> {
        Ok(self.account.lock().expect("lock").clone())
    }

    async fn place(&self, ticket: &OrderTicket) -> Result<PlaceResult, BrokerError> {
        if *self.fail_place.lock().expect("lock")
            || (*self.fail_buys.lock().expect("lock") && ticket.side == moon_core::types::Side::Buy)
        {
            return Err(BrokerError::Broker("timeout de red".into()));
        }
        if *self.reject_place.lock().expect("lock")
            || *self.reject_type.lock().expect("lock") == Some(ticket.order_type)
            || (*self.reject_buys.lock().expect("lock")
                && ticket.side == moon_core::types::Side::Buy)
        {
            return Ok(PlaceResult {
                broker_order_id: "rejected".into(),
                accepted: false,
                messages: vec!["rechazada por el mercado".into()],
                fill: None,
            });
        }
        let mut p = self.places.lock().expect("lock");
        p.push(ticket.clone());
        let mut fill = None;
        let only_sells = *self.fill_only_sells.lock().expect("lock");
        let fill_cfg = (*self.fill_on_place.lock().expect("lock"))
            .filter(|_| !only_sells || ticket.side == moon_core::types::Side::Sell);
        if let Some(fees) = fill_cfg {
            let price = ticket.limit_price.unwrap_or(dec!(10000));
            let value = ticket.qty * price;
            let mut a = self.account.lock().expect("lock");
            let held = a.positions.get(&ticket.symbol).copied().unwrap_or_default();
            let (new_held, cash) = match ticket.side {
                moon_core::types::Side::Buy => (held + ticket.qty, a.cash_ars - value),
                moon_core::types::Side::Sell if *self.cash_lag.lock().expect("lock") => {
                    *self.pending_cash.lock().expect("lock") += value;
                    (held - ticket.qty, a.cash_ars)
                }
                moon_core::types::Side::Sell => (held - ticket.qty, a.cash_ars + value),
            };
            a.cash_ars = cash;
            a.cash_total_ars = cash;
            a.positions.insert(ticket.symbol.clone(), new_held);
            a.sellable.insert(ticket.symbol.clone(), new_held);
            fill = Some(OrderProgress {
                status: OrderStatus::Filled,
                filled_qty: ticket.qty,
                avg_price: Some(price),
                fees,
                listing: Listing::Listed,
            });
        }
        Ok(PlaceResult {
            broker_order_id: format!("fake-{}", p.len()),
            accepted: true,
            messages: vec![],
            fill,
        })
    }

    async fn refresh(
        &self,
        broker_order_id: &str,
        symbol: &str,
        _placed_on: NaiveDate,
    ) -> Result<OrderProgress, BrokerError> {
        self.refreshes
            .lock()
            .expect("lock")
            .push((broker_order_id.to_string(), symbol.to_string()));
        if *self.fail_refresh.lock().expect("lock") {
            return Err(BrokerError::Broker("IOL no responde".into()));
        }
        if self
            .cancelled
            .lock()
            .expect("lock")
            .contains(broker_order_id)
        {
            // A cancelled order keeps whatever was filled before the cancel.
            let mut p = self.refresh_result.lock().expect("lock").clone();
            p.status = OrderStatus::Cancelled;
            return Ok(p);
        }
        if let Some(listing) = *self.not_listed.lock().expect("lock") {
            return Ok(OrderProgress {
                status: OrderStatus::Sent,
                filled_qty: Decimal::ZERO,
                avg_price: None,
                fees: None,
                listing,
            });
        }
        let threshold = *self.fill_on_refresh.lock().expect("lock");
        if let Some(n) = threshold {
            let count = {
                let mut c = self.refresh_counts.lock().expect("lock");
                let e = c.entry(broker_order_id.to_string()).or_insert(0);
                *e += 1;
                *e
            };
            let ticket = broker_order_id
                .strip_prefix("fake-")
                .and_then(|i| i.parse::<usize>().ok())
                .and_then(|i| i.checked_sub(1))
                .and_then(|i| self.places.lock().expect("lock").get(i).cloned());
            if let (true, Some(t)) = (count >= n, ticket) {
                let price = t.limit_price.unwrap_or(dec!(10000));
                if count == n {
                    let value = t.qty * price;
                    let mut a = self.account.lock().expect("lock");
                    let held = a.positions.get(&t.symbol).copied().unwrap_or_default();
                    let (new_held, cash) = match t.side {
                        moon_core::types::Side::Buy => (held + t.qty, a.cash_ars - value),
                        moon_core::types::Side::Sell => (held - t.qty, a.cash_ars + value),
                    };
                    a.cash_ars = cash;
                    a.cash_total_ars = cash;
                    a.positions.insert(t.symbol.clone(), new_held);
                    a.sellable.insert(t.symbol.clone(), new_held);
                }
                return Ok(OrderProgress {
                    status: OrderStatus::Filled,
                    filled_qty: t.qty,
                    avg_price: Some(price),
                    fees: None,
                    listing: Listing::Listed,
                });
            }
        }
        Ok(self.refresh_result.lock().expect("lock").clone())
    }

    async fn cancel(&self, broker_order_id: &str) -> Result<(), BrokerError> {
        self.cancels
            .lock()
            .expect("lock")
            .push(broker_order_id.to_string());
        if *self.cancel_fails.lock().expect("lock") {
            return Err(BrokerError::Broker(
                "IOL no confirmó la cancelación de la orden".into(),
            ));
        }
        if *self.cancel_confirms.lock().expect("lock") {
            self.cancelled
                .lock()
                .expect("lock")
                .insert(broker_order_id.to_string());
        }
        Ok(())
    }

    async fn external_flows(
        &self,
        _from: NaiveDate,
        _to: NaiveDate,
    ) -> Result<Vec<ExternalFlow>, BrokerError> {
        if *self.fail_flows.lock().expect("lock") {
            return Err(BrokerError::Broker("flujos no disponibles".into()));
        }
        Ok(self.flows.lock().expect("lock").clone())
    }
}

pub struct Harness {
    pub engine: Engine,
    pub store: Arc<Mutex<Store>>,
    pub quotes: Arc<StaticQuotes>,
    pub notifier: Arc<RecordingNotifier>,
    pub clock: Arc<FixedClock>,
    pub fake: Option<Arc<FakeBroker>>,
}

fn parts(
    cfg: &AppConfig,
) -> (
    Arc<Mutex<Store>>,
    Arc<StaticQuotes>,
    Arc<RecordingNotifier>,
    Arc<FixedClock>,
) {
    let mut store = Store::open_in_memory().expect("store");
    seed_series(&mut store, cfg);
    // Quotes are stamped with the test clock so they count as fresh.
    let quotes = Arc::new(StaticQuotes::with_stamp(clock_time()));
    seed_quotes(&quotes, cfg);
    (
        Arc::new(Mutex::new(store)),
        quotes,
        Arc::new(RecordingNotifier::new()),
        Arc::new(FixedClock::new(clock_time())),
    )
}

impl Harness {
    /// Paper mode with the given initial cash in the paper account.
    pub fn paper(initial_cash: Decimal) -> Harness {
        Self::paper_with(initial_cash, |_| {})
    }

    /// Like [`Harness::paper`], letting the test adjust the configuration first.
    pub fn paper_with(initial_cash: Decimal, tweak: impl FnOnce(&mut AppConfig)) -> Harness {
        let mut cfg = test_config(TradingMode::Paper);
        tweak(&mut cfg);
        cfg.validate().expect("tweaked config valid");
        let (store, quotes, notifier, clock) = parts(&cfg);
        store
            .lock()
            .expect("lock")
            .save_paper_account(&PaperAccount {
                cash_ars: initial_cash,
                positions: BTreeMap::new(),
            })
            .expect("save paper account");
        let broker = Arc::new(PaperBroker::new(
            store.clone(),
            quotes.clone(),
            cfg.fees.clone(),
            cfg.execution.slippage_rate,
            clock.clone(),
        ));
        let engine = Engine::new(
            cfg,
            EngineDeps {
                store: store.clone(),
                broker,
                quotes: quotes.clone(),
                notifier: notifier.clone(),
                clock: clock.clone(),
            },
        )
        .expect("engine");
        Harness {
            engine,
            store,
            quotes,
            notifier,
            clock,
            fake: None,
        }
    }

    /// Live mode (opted in) on top of a [`FakeBroker`] with the given cash.
    pub fn live(mode: TradingMode, cash_ars: Decimal) -> Harness {
        Self::live_with(mode, cash_ars, |_| {})
    }

    /// Like [`Harness::live`], letting the test adjust the configuration first.
    pub fn live_with(
        mode: TradingMode,
        cash_ars: Decimal,
        tweak: impl FnOnce(&mut AppConfig),
    ) -> Harness {
        let mut cfg = test_config(mode);
        tweak(&mut cfg);
        cfg.validate().expect("tweaked config valid");
        let (store, quotes, notifier, clock) = parts(&cfg);
        let fake = Arc::new(FakeBroker::new(cash_ars));
        let engine = Engine::new(
            cfg,
            EngineDeps {
                store: store.clone(),
                broker: fake.clone(),
                quotes: quotes.clone(),
                notifier: notifier.clone(),
                clock: clock.clone(),
            },
        )
        .expect("engine");
        Harness {
            engine,
            store,
            quotes,
            notifier,
            clock,
            fake: Some(fake),
        }
    }

    pub fn fake(&self) -> &Arc<FakeBroker> {
        self.fake.as_ref().expect("live harness")
    }
}

/// Builds an engine from an arbitrary config over a fake broker; used to test construction errors.
pub fn try_engine(cfg: AppConfig) -> anyhow::Result<Engine> {
    let (store, quotes, notifier, clock) = parts(&test_config(TradingMode::Paper));
    Engine::new(
        cfg,
        EngineDeps {
            store,
            broker: Arc::new(FakeBroker::new(dec!(1000))),
            quotes,
            notifier,
            clock,
        },
    )
}
