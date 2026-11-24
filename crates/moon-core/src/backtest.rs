//! Backtest engine: monthly decisions, daily valuation, emergency exits, contributions and costs.
//!
//! Reuses the live allocator, rebalancer and risk code. Decisions only see data up to the day
//! before the date on which their orders execute (at the open).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, Duration, NaiveDate};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

use crate::allocator::{AllocationDecision, Allocator};
use crate::asset::{AssetId, AssetSpec, Universe};
use crate::bars::PriceSeries;
use crate::error::CoreError;
use crate::fees::FeeModel;
use crate::metrics::{Metrics, compute_metrics, nav_metrics};
use crate::money::{dec_from_f64, dec_to_f64, round_money};
use crate::nav::NavTracker;
use crate::rebalance::{RebalanceInput, RebalanceParams, plan_rebalance};
use crate::risk::{DrawdownSignal, DrawdownState, RiskLimits};
use crate::risk_index::RiskIndex;
use crate::strategy::SignalContext;
use crate::types::{OrderReason, Side};
use crate::weights::TargetWeights;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BacktestConfig {
    pub start: NaiveDate,
    #[serde(default)]
    pub end: Option<NaiveDate>,
    pub initial_capital: Decimal,
    #[serde(default)]
    pub monthly_contribution: Decimal,
    pub slippage_rate: Decimal,
    pub fees: FeeModel,
    #[serde(default)]
    pub holdout_start: Option<NaiveDate>,
    #[serde(default)]
    pub risk_free_rate: f64,
    /// Annual return of idle cash in USD terms (default 0). Negative values model peso cash
    /// losing value against the dollar. Accrued per trading day.
    #[serde(default)]
    pub cash_usd_return: f64,
}

pub struct BacktestInput<'a> {
    pub config: &'a BacktestConfig,
    pub risk: &'a RiskLimits,
    pub universe: &'a Universe,
    pub allocator: &'a Allocator,
    pub series: &'a BTreeMap<AssetId, PriceSeries>,
    pub include_holdout: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EquityPoint {
    pub date: NaiveDate,
    pub equity: Decimal,
    pub cash: Decimal,
    pub flow: Decimal,
    pub nav: f64,
    pub drawdown: f64,
    pub refuge_weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub date: NaiveDate,
    pub asset: AssetId,
    pub side: Side,
    pub qty: Decimal,
    pub price: Decimal,
    pub value: Decimal,
    pub fees: Decimal,
    pub reason: OrderReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestResult {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub equity_curve: Vec<EquityPoint>,
    pub trades: Vec<Trade>,
    pub decisions: Vec<AllocationDecision>,
    pub emergency_exits: Vec<NaiveDate>,
    pub metrics: Metrics,
    pub benchmark: Option<Metrics>,
}

/// Unit price (per underlying share equivalent of one CEDEAR) on `date`.
/// At the open uses the adjusted open; otherwise the adjusted close. Without a bar on `date`
/// the last earlier adjusted close is used.
fn unit_price(series: &PriceSeries, ratio: f64, date: NaiveDate, at_open: bool) -> Option<Decimal> {
    let raw = match series.bar_on(date) {
        Some(b) if at_open => b.adj_open(),
        Some(b) => b.adj_close,
        None => series.on_or_before(date)?.adj_close,
    };
    let px = dec_from_f64(raw / ratio)?
        .round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero);
    (px > Decimal::ZERO).then_some(px)
}

/// Number of calendar months with at least one bar dated before `date`.
fn months_before(month_firsts: &[NaiveDate], date: NaiveDate) -> usize {
    month_firsts.partition_point(|d| *d < date)
}

/// Date of the first bar of each calendar month.
fn month_first_dates(series: &PriceSeries) -> Vec<NaiveDate> {
    let mut out: Vec<NaiveDate> = Vec::new();
    for b in series.bars() {
        match out.last() {
            Some(l) if (l.year(), l.month()) == (b.date.year(), b.date.month()) => {}
            _ => out.push(b.date),
        }
    }
    out
}

fn missing(msg: impl Into<String>) -> CoreError {
    CoreError::MissingData(msg.into())
}

struct Priced<'a> {
    spec: &'a AssetSpec,
    series: &'a PriceSeries,
}

pub fn run_backtest(input: &BacktestInput<'_>) -> Result<BacktestResult, CoreError> {
    let cfg = input.config;
    let universe = input.universe;
    input.risk.validate()?;
    cfg.fees.validate()?;
    if cfg.monthly_contribution < Decimal::ZERO {
        return Err(CoreError::InvalidConfig(
            "el aporte mensual debe ser >= 0".into(),
        ));
    }
    if cfg.initial_capital <= Decimal::ZERO {
        return Err(CoreError::InvalidConfig(
            "el capital inicial debe ser > 0".into(),
        ));
    }
    if !cfg.cash_usd_return.is_finite() || cfg.cash_usd_return <= -1.0 {
        return Err(CoreError::InvalidConfig(
            "cash_usd_return debe ser > -1".into(),
        ));
    }
    let refuge = universe.refuge().id.clone();

    // Tradable assets (non-CASH) with their series.
    let mut priced: Vec<Priced<'_>> = Vec::new();
    for spec in universe.tradable() {
        let series = input
            .series
            .get(&spec.id)
            .ok_or_else(|| missing(format!("falta la serie de {}", spec.id)))?;
        priced.push(Priced { spec, series });
    }

    // Effective start: enough history for every signal.
    let mut required: Vec<&AssetSpec> = universe.risky();
    required.extend(universe.canaries());
    if !refuge.is_cash() {
        required.push(universe.refuge());
    }
    let warmup = input.allocator.warmup_months();
    let firsts: Vec<Vec<NaiveDate>> = required
        .iter()
        .map(|s| {
            input
                .series
                .get(&s.id)
                .map(month_first_dates)
                .unwrap_or_default()
        })
        .collect();

    let all_dates: BTreeSet<NaiveDate> = priced
        .iter()
        .flat_map(|p| p.series.bars().iter().map(|b| b.date))
        .filter(|d| *d >= cfg.start)
        .collect();
    let eff_start = all_dates
        .iter()
        .copied()
        .find(|d| firsts.iter().all(|f| months_before(f, *d) >= warmup))
        .ok_or_else(|| missing("historial insuficiente para el período del backtest"))?;

    let mut eff_end = cfg
        .end
        .or_else(|| all_dates.iter().next_back().copied())
        .ok_or_else(|| missing("no hay datos de precios"))?;
    if let (false, Some(h)) = (input.include_holdout, cfg.holdout_start) {
        eff_end = eff_end.min(h - Duration::days(1));
    }
    if eff_end <= eff_start {
        return Err(missing("el período del backtest no contiene datos"));
    }
    let calendar: Vec<NaiveDate> = all_dates.range(eff_start..=eff_end).copied().collect();
    if calendar.len() < 2 {
        return Err(missing("el período del backtest no contiene datos"));
    }

    let lots: BTreeMap<AssetId, u32> = priced
        .iter()
        .map(|p| {
            (
                p.spec.id.clone(),
                p.spec.exec.as_ref().map_or(1, |e| e.lot_size),
            )
        })
        .collect();
    let prices_on = |date: NaiveDate, at_open: bool| -> BTreeMap<AssetId, Decimal> {
        priced
            .iter()
            .filter_map(|p| {
                unit_price(p.series, p.spec.cedear_ratio, date, at_open)
                    .map(|px| (p.spec.id.clone(), px))
            })
            .collect()
    };

    let mut cash = cfg.initial_capital;
    let mut holdings: BTreeMap<AssetId, Decimal> = BTreeMap::new();
    let mut tracker = NavTracker::start(cfg.initial_capital);
    // The breaker watches the invested part only (cash excluded), in USD.
    let mut risk_index = RiskIndex::new();
    let mut drawdown = DrawdownState {
        peak_nav: 1.0,
        ..Default::default()
    };
    let daily_cash_factor = dec_from_f64((1.0 + cfg.cash_usd_return).powf(1.0 / 252.0));
    let mut pending_emergency = false;
    let mut prev_equity = cfg.initial_capital;

    let mut curve: Vec<EquityPoint> = Vec::with_capacity(calendar.len());
    let mut trades: Vec<Trade> = Vec::new();
    let mut decisions: Vec<AllocationDecision> = Vec::new();
    let mut emergency_exits: Vec<NaiveDate> = Vec::new();

    for (i, &date) in calendar.iter().enumerate() {
        if i > 0
            && cfg.cash_usd_return != 0.0
            && cash > Decimal::ZERO
            && let Some(f) = daily_cash_factor
        {
            cash = round_money(cash * f);
        }
        let mut flow = if i == 0 {
            cfg.initial_capital
        } else {
            Decimal::ZERO
        };
        let first_of_month = i == 0 || {
            let p = calendar[i - 1];
            (p.year(), p.month()) != (date.year(), date.month())
        };

        let (targets, reason, force_full) = if first_of_month {
            if i > 0 && cfg.monthly_contribution != Decimal::ZERO {
                flow += cfg.monthly_contribution;
                tracker.apply_flow(flow, prev_equity);
                cash += flow;
            }
            let as_of = if i == 0 {
                date - Duration::days(1)
            } else {
                calendar[i - 1]
            };
            let decision =
                input
                    .allocator
                    .allocate(&SignalContext::new(as_of, universe, input.series));
            let targets = decision.final_weights.clone();
            decisions.push(decision);
            // Only the first decision after a trip re-arms the breaker.
            drawdown.monthly_decision(risk_index.level);
            pending_emergency = false;
            (Some(targets), OrderReason::Monthly, false)
        } else if pending_emergency {
            pending_emergency = false;
            emergency_exits.push(date);
            (
                Some(TargetWeights::all_in(refuge.clone())),
                OrderReason::Emergency,
                true,
            )
        } else {
            (None, OrderReason::Monthly, false)
        };

        if let Some(targets) = targets {
            let open_prices = prices_on(date, true);
            let params = RebalanceParams {
                band: input.risk.rebalance_band,
                min_trade_value: input.risk.min_trade_usd,
                cash_buffer: input.risk.cash_buffer,
                slippage_rate: cfg.slippage_rate,
                force_full,
            };
            let plan = plan_rebalance(&RebalanceInput {
                cash,
                holdings: &holdings,
                prices: &open_prices,
                lot_sizes: &lots,
                targets: &targets,
                fees: &cfg.fees,
                params: &params,
            })?;
            for o in &plan.orders {
                match o.side {
                    Side::Buy => {
                        cash -= o.est_value + o.est_fees;
                        *holdings.entry(o.asset.clone()).or_insert(Decimal::ZERO) += o.qty;
                    }
                    Side::Sell => {
                        cash += o.est_value - o.est_fees;
                        let left = holdings.entry(o.asset.clone()).or_insert(Decimal::ZERO);
                        *left -= o.qty;
                        if *left <= Decimal::ZERO {
                            holdings.remove(&o.asset);
                        }
                    }
                }
                trades.push(Trade {
                    date,
                    asset: o.asset.clone(),
                    side: o.side,
                    qty: o.qty,
                    price: o.exec_price,
                    value: o.est_value,
                    fees: o.est_fees,
                    reason,
                });
            }
        }

        // Close: value holdings at adjusted close.
        let close_prices = prices_on(date, false);
        let mut refuge_value = Decimal::ZERO;
        let mut equity = cash;
        for (id, qty) in &holdings {
            let px = close_prices
                .get(id)
                .copied()
                .ok_or_else(|| CoreError::MissingPrice(id.clone()))?;
            let v = *qty * px;
            equity += v;
            if *id == refuge {
                refuge_value += v;
            }
        }
        if refuge.is_cash() {
            refuge_value = cash;
        }
        let nav = tracker
            .nav(equity)
            .ok_or_else(|| missing("no hay unidades de NAV para valuar la cartera"))?;
        let held: BTreeMap<String, Decimal> = holdings
            .iter()
            .map(|(id, q)| (id.to_string(), *q))
            .collect();
        let usd_prices: BTreeMap<String, Decimal> = close_prices
            .iter()
            .map(|(id, px)| (id.to_string(), *px))
            .collect();
        let at = date.and_hms_opt(20, 0, 0).unwrap_or_default().and_utc();
        risk_index.observe(&held, &usd_prices, at)?;
        let level = risk_index.level;
        if drawdown.observe(level, date, input.risk.max_drawdown) == DrawdownSignal::Tripped {
            pending_emergency = true;
        }
        let refuge_weight = if equity > Decimal::ZERO {
            dec_to_f64(refuge_value / equity)
        } else {
            0.0
        };
        curve.push(EquityPoint {
            date,
            equity: round_money(equity),
            cash: round_money(cash),
            flow,
            nav,
            drawdown: drawdown.drawdown(level),
            refuge_weight,
        });
        prev_equity = equity;
    }

    let metrics = compute_metrics(&curve, &trades, cfg.risk_free_rate);
    let benchmark = universe
        .benchmark()
        .and_then(|b| input.series.get(&b.id))
        .and_then(|s| {
            let closes: Vec<(NaiveDate, f64)> = calendar
                .iter()
                .filter_map(|d| s.on_or_before(*d).map(|b| (*d, b.adj_close)))
                .collect();
            let base = closes.first()?.1;
            let navs: Vec<(NaiveDate, f64)> =
                closes.into_iter().map(|(d, c)| (d, c / base)).collect();
            Some(nav_metrics(&navs, cfg.risk_free_rate))
        });

    Ok(BacktestResult {
        start: calendar[0],
        end: calendar[calendar.len() - 1],
        equity_curve: curve,
        trades,
        decisions,
        emergency_exits,
        metrics,
        benchmark,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::{AllocationConfig, SizingKind, StrategyConfig};
    use crate::asset::Role;
    use crate::bars::Bar;
    use crate::testutil::{d, daily_series, spec};
    use rust_decimal_macros::dec;

    fn risk() -> RiskLimits {
        RiskLimits {
            max_weight: 1.0,
            max_drawdown: 0.15,
            rebalance_band: 0.0,
            min_trade_usd: dec!(0),
            max_order_usd: dec!(1000000),
            max_orders_per_day: 10,
            cash_buffer: 0.0,
            max_equity_usd: None,
            trip_confirmations: 2,
        }
    }

    fn config() -> BacktestConfig {
        BacktestConfig {
            start: d(2023, 1, 2),
            end: None,
            initial_capital: dec!(10000),
            monthly_contribution: dec!(0),
            slippage_rate: dec!(0),
            fees: FeeModel::zero(),
            holdout_start: None,
            risk_free_rate: 0.0,
            cash_usd_return: 0.0,
        }
    }

    fn universe() -> Universe {
        Universe::new(vec![spec("A", Role::Risky), spec("CASH", Role::Refuge)]).unwrap()
    }

    fn allocator() -> Allocator {
        Allocator::new(
            &AllocationConfig {
                strategies: vec![StrategyConfig::FaberSma { months: 10 }],
                sizing: SizingKind::Equal,
                vol_lookback_days: 63,
                canary_threshold: None,
            },
            1.0,
        )
        .unwrap()
    }

    fn series_a(closes: &[f64]) -> BTreeMap<AssetId, PriceSeries> {
        let mut m = BTreeMap::new();
        m.insert(AssetId::new("A"), daily_series(d(2022, 1, 3), closes));
        m
    }

    fn rising(n: usize) -> Vec<f64> {
        (0..n).map(|i| 100.0 * 1.0005_f64.powi(i as i32)).collect()
    }

    fn run(
        cfg: &BacktestConfig,
        series: &BTreeMap<AssetId, PriceSeries>,
        include_holdout: bool,
    ) -> Result<BacktestResult, CoreError> {
        let u = universe();
        let a = allocator();
        let r = risk();
        run_backtest(&BacktestInput {
            config: cfg,
            risk: &r,
            universe: &u,
            allocator: &a,
            series,
            include_holdout,
        })
    }

    #[test]
    fn buy_and_hold_like_when_always_risky() {
        let series = series_a(&rising(900));
        let res = run(&config(), &series, false).unwrap();
        let first = &res.equity_curve[0];
        assert!(first.refuge_weight < 0.01, "refuge {}", first.refuge_weight);
        let bars = series[&AssetId::new("A")].bars();
        let p0 = bars.iter().find(|b| b.date == res.start).unwrap().open;
        let p1 = bars.iter().find(|b| b.date == res.end).unwrap().adj_close;
        let bh = 10000.0 * p1 / p0;
        let fin = dec_to_f64(res.metrics.final_equity);
        assert!((fin / bh - 1.0).abs() < 0.01, "final {fin} vs bh {bh}");
    }

    #[test]
    fn decisions_never_use_future_data() {
        let cutoff = d(2024, 3, 15);
        let full = series_a(&rising(900));
        let mut tampered = BTreeMap::new();
        for (id, s) in &full {
            let bars: Vec<Bar> = s
                .bars()
                .iter()
                .map(|b| {
                    let mut b = b.clone();
                    if b.date >= cutoff {
                        b.open *= 10.0;
                        b.high *= 10.0;
                        b.low *= 10.0;
                        b.close *= 10.0;
                        b.adj_close *= 10.0;
                    }
                    b
                })
                .collect();
            tampered.insert(id.clone(), PriceSeries::new(bars).unwrap());
        }
        let a = run(&config(), &full, false).unwrap();
        let b = run(&config(), &tampered, false).unwrap();
        let da: Vec<_> = a.decisions.iter().filter(|x| x.as_of < cutoff).collect();
        let db: Vec<_> = b.decisions.iter().filter(|x| x.as_of < cutoff).collect();
        assert!(da.len() > 3);
        assert_eq!(da, db);
        let ta: Vec<_> = a.trades.iter().filter(|t| t.date < cutoff).collect();
        let tb: Vec<_> = b.trades.iter().filter(|t| t.date < cutoff).collect();
        assert!(!ta.is_empty());
        assert_eq!(ta, tb);
    }

    #[test]
    fn fees_reduce_equity() {
        let series = series_a(&rising(900));
        let free = run(&config(), &series, false).unwrap();
        let mut cfg = config();
        cfg.fees = FeeModel {
            commission_rate: dec!(0.006),
            market_fee_rate: dec!(0),
            vat_rate: dec!(0),
            min_commission: dec!(0),
        };
        let paid = run(&cfg, &series, false).unwrap();
        assert!(paid.metrics.final_equity < free.metrics.final_equity);
        assert!(paid.metrics.total_fees > Decimal::ZERO);
        assert_eq!(free.metrics.total_fees, Decimal::ZERO);
    }

    #[test]
    fn contributions_do_not_change_nav() {
        let series = series_a(&vec![100.0; 900]);
        let mut cfg = config();
        cfg.monthly_contribution = dec!(100);
        let res = run(&cfg, &series, false).unwrap();
        for p in &res.equity_curve {
            assert!((p.nav - 1.0).abs() < 1e-9, "nav {} on {}", p.nav, p.date);
        }
        let last = res.equity_curve.last().unwrap();
        assert!(last.equity > dec!(10000));
        let n = res
            .equity_curve
            .iter()
            .skip(1)
            .filter(|p| p.flow > dec!(0))
            .count();
        assert!(n > 10);
        assert_eq!(res.equity_curve[0].flow, dec!(10000));
        assert_eq!(
            res.metrics.net_contributions,
            dec!(10000) + dec!(100) * Decimal::from(n)
        );
        assert_eq!(last.equity, res.metrics.net_contributions);
    }

    #[test]
    fn crash_triggers_emergency_exit() {
        let n = 900;
        let probe = daily_series(d(2022, 1, 3), &vec![1.0; n]);
        let crash_date = d(2023, 8, 16);
        let recover_date = d(2023, 9, 5);
        let base = rising(n);
        let closes: Vec<f64> = probe
            .bars()
            .iter()
            .zip(base)
            .map(|(b, c)| {
                if b.date >= recover_date {
                    c * 1.05
                } else if b.date >= crash_date {
                    c * 0.7
                } else {
                    c
                }
            })
            .collect();
        let series = series_a(&closes);
        let res = run(&config(), &series, false).unwrap();
        assert!(!res.emergency_exits.is_empty());
        let ex = res.emergency_exits[0];
        assert!(ex > crash_date);
        let on_day: Vec<&Trade> = res.trades.iter().filter(|t| t.date == ex).collect();
        assert!(!on_day.is_empty());
        assert!(
            on_day
                .iter()
                .all(|t| t.reason == OrderReason::Emergency && t.side == Side::Sell)
        );
        assert!(res.decisions.iter().any(|dc| dc.as_of >= ex));
        // The next monthly rebalance re-evaluates normally and buys back in.
        assert!(
            res.trades
                .iter()
                .any(|t| t.date > ex && t.reason == OrderReason::Monthly && t.side == Side::Buy)
        );
    }

    #[test]
    fn holdout_excluded_by_default() {
        let series = series_a(&rising(900));
        let mut cfg = config();
        cfg.holdout_start = Some(d(2024, 6, 3));
        let res = run(&cfg, &series, false).unwrap();
        assert!(res.end < d(2024, 6, 3));
        let res = run(&cfg, &series, true).unwrap();
        assert!(res.end >= d(2024, 6, 3));
    }

    #[test]
    fn insufficient_history_errors() {
        // Start lies inside the data, so only the warmup rule can fail.
        let mut cfg = config();
        cfg.start = d(2022, 1, 3);
        let series = series_a(&rising(65));
        let err = run(&cfg, &series, false).unwrap_err();
        assert!(matches!(err, CoreError::MissingData(_)), "{err:?}");
    }

    #[test]
    fn warmup_just_satisfied_runs() {
        // Faber 10m needs 11 monthly closes: Jan..Oct plus the partial Nov (bar on 11-01) precede 11-02.
        let mut cfg = config();
        cfg.start = d(2022, 1, 3);
        let series = series_a(&rising(900));
        let res = run(&cfg, &series, false).unwrap();
        assert_eq!(res.start, d(2022, 11, 2));
    }

    #[test]
    fn invalid_contribution_is_rejected() {
        let mut cfg = config();
        cfg.monthly_contribution = dec!(-1);
        let series = series_a(&rising(900));
        assert!(matches!(
            run(&cfg, &series, false),
            Err(CoreError::InvalidConfig(_))
        ));
    }

    #[test]
    fn benchmark_is_reported_when_present() {
        let u = Universe::new(vec![
            spec("A", Role::Risky),
            spec("CASH", Role::Refuge),
            spec("BM", Role::Benchmark),
        ])
        .unwrap();
        let mut series = series_a(&rising(900));
        series.insert(
            AssetId::new("BM"),
            daily_series(d(2022, 1, 3), &rising(900)),
        );
        let a = allocator();
        let r = risk();
        let cfg = config();
        let res = run_backtest(&BacktestInput {
            config: &cfg,
            risk: &r,
            universe: &u,
            allocator: &a,
            series: &series,
            include_holdout: false,
        })
        .unwrap();
        assert!(res.benchmark.is_some_and(|b| b.total_return > 0.0));
    }

    #[test]
    fn slow_multi_month_decline_trips_the_breaker() {
        // Strong rally, then -4 % per month: no single month is near the 15 % limit, but the
        // running peak is, so the breaker must trip.
        let n = 1300;
        let rally_days = 900;
        let monthly = 0.96_f64.powf(1.0 / 21.0);
        let mut px = 100.0_f64;
        let closes: Vec<f64> = (0..n)
            .map(|i| {
                px *= if i < rally_days { 1.006 } else { monthly };
                px
            })
            .collect();
        let series = series_a(&closes);
        let res = run(&config(), &series, false).unwrap();
        assert!(
            !res.emergency_exits.is_empty(),
            "max dd {:?}",
            res.equity_curve
                .iter()
                .map(|p| p.drawdown)
                .fold(0.0_f64, f64::max)
        );
        let peak_nav = res
            .equity_curve
            .iter()
            .map(|p| p.nav)
            .fold(0.0_f64, f64::max);
        assert!(peak_nav > 1.0);
    }

    #[test]
    fn cash_usd_return_erodes_idle_cash() {
        // A falling series keeps the Faber signal out of A: the portfolio is all CASH.
        let closes: Vec<f64> = (0..900).map(|i| 200.0 * 0.999_f64.powi(i)).collect();
        let series = series_a(&closes);
        let mut cfg = config();
        cfg.cash_usd_return = -0.25;
        let res = run(&cfg, &series, false).unwrap();
        assert!(res.trades.is_empty(), "trades {:?}", res.trades);
        let days = (res.equity_curve.len() - 1) as f64;
        let expected = 10000.0 * 0.75_f64.powf(days / 252.0);
        let fin = dec_to_f64(res.equity_curve.last().unwrap().equity);
        assert!((fin - expected).abs() < 5.0, "final {fin} vs {expected}");
        assert!(days > 252.0 * 0.9);
    }

    #[test]
    fn invalid_cash_usd_return_is_rejected() {
        let mut cfg = config();
        cfg.cash_usd_return = -1.0;
        let series = series_a(&rising(900));
        assert!(matches!(
            run(&cfg, &series, false),
            Err(CoreError::InvalidConfig(_))
        ));
    }
}
