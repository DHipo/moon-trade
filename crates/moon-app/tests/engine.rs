mod common;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use common::{Harness, TODAY, clock_time, test_config, try_engine};
use moon_app::config::TradingMode;
use moon_app::engine::{
    MonthlyOutcome, STATE_DRAWDOWN, STATE_NAV, STATE_RISK_INDEX, UNKNOWN_OUTCOME, order_id_for,
    previous_trading_day,
};
use moon_broker::ExternalFlow;
use moon_core::asset::{Currency, Market};
use moon_core::nav::NavTracker;
use moon_core::risk::DrawdownState;
use moon_core::risk_index::RiskIndex;
use moon_core::types::{ExecMode, OrderReason, OrderStatus, Side};
use moon_store::{FlowKind, ProposalStatus};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::BTreeMap;
use uuid::Uuid;

fn far_past() -> DateTime<Utc> {
    clock_time() - Duration::days(3650)
}

fn far_future() -> DateTime<Utc> {
    clock_time() + Duration::days(3650)
}

fn all_orders(h: &Harness, mode: ExecMode) -> Vec<moon_store::OrderRecord> {
    h.store
        .lock()
        .expect("lock")
        .orders_between(mode, far_past(), far_future())
        .expect("orders")
}

#[tokio::test]
async fn paper_monthly_rebalance_buys_from_cash() {
    let h = Harness::paper(dec!(1000000));
    let out = h.engine.run_monthly(TODAY).await.expect("monthly");
    let MonthlyOutcome::Executed(report) = out else {
        panic!("expected Executed, got {out:?}");
    };
    assert!(!report.orders.is_empty());
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(
        report
            .orders
            .iter()
            .all(|o| o.status == OrderStatus::Filled && o.side == Side::Buy)
    );
    let store = h.store.lock().expect("lock");
    let acct = store.paper_account().expect("account");
    assert!(!acct.positions.is_empty());
    assert!(acct.cash_ars >= Decimal::ZERO);
    assert!(
        !store
            .recent_decisions(5, ExecMode::Paper)
            .expect("decisions")
            .is_empty()
    );
}

#[tokio::test]
async fn live_confirm_creates_proposal_then_executes_on_approval() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    let out = h.engine.run_monthly(TODAY).await.expect("monthly");
    let MonthlyOutcome::ProposalSent(id) = out else {
        panic!("expected ProposalSent, got {out:?}");
    };
    assert!(
        h.fake().places().is_empty(),
        "nothing is placed before approval"
    );
    assert_eq!(h.notifier.approvals(), vec![id]);

    let text = h.engine.handle_approval(id, true).await.expect("approve");
    assert!(!h.fake().places().is_empty(), "{text}");
    let p = h.store.lock().expect("lock").get_proposal(id).expect("get");
    assert_eq!(p.expect("proposal").status, ProposalStatus::Executed);

    let placed = h.fake().places().len();
    let again = h.engine.handle_approval(id, true).await.expect("again");
    assert!(again.contains("ya fue resuelta"), "{again}");
    assert_eq!(h.fake().places().len(), placed);
}

#[tokio::test]
async fn rejected_proposal_places_nothing() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    let MonthlyOutcome::ProposalSent(id) = h.engine.run_monthly(TODAY).await.expect("monthly")
    else {
        panic!("expected a proposal");
    };
    h.engine.handle_approval(id, false).await.expect("reject");
    assert!(h.fake().places().is_empty());
    let p = h.store.lock().expect("lock").get_proposal(id).expect("get");
    assert_eq!(p.expect("proposal").status, ProposalStatus::Rejected);
    assert!(all_orders(&h, ExecMode::Live).is_empty());
}

#[tokio::test]
async fn expired_proposal_is_not_executed() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    let MonthlyOutcome::ProposalSent(id) = h.engine.run_monthly(TODAY).await.expect("monthly")
    else {
        panic!("expected a proposal");
    };
    h.clock.advance(Duration::hours(6));
    let text = h.engine.handle_approval(id, true).await.expect("approve");
    assert!(text.contains("venció"), "{text}");
    assert!(h.fake().places().is_empty());
    let p = h.store.lock().expect("lock").get_proposal(id).expect("get");
    assert_eq!(p.expect("proposal").status, ProposalStatus::Expired);
}

#[tokio::test]
async fn unknown_proposal_is_reported() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    let text = h
        .engine
        .handle_approval(Uuid::new_v4(), true)
        .await
        .expect("approval");
    assert_eq!(text, "Propuesta inexistente.");
}

#[tokio::test]
async fn emergency_exit_ignores_pause_and_confirmation() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(0));
    h.engine.set_paused(true).expect("pause");
    arm_breaker(&h);
    let out = h.engine.run_risk_check().await.expect("risk check");
    assert!(out.tripped);
    assert!(out.drawdown >= 0.15);
    let report = out.emergency.expect("emergency executed");
    let places = h.fake().places();
    assert_eq!(places.len(), 1);
    assert_eq!(places[0].side, Side::Sell);
    assert_eq!(places[0].qty, dec!(100));
    assert_eq!(report.orders[0].reason, OrderReason::Emergency);
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("Freno por caída"))
    );
    assert!(h.notifier.approvals().is_empty());

    // The breaker stays latched: no second emergency.
    let again = h.engine.run_risk_check().await.expect("second check");
    assert!(!again.tripped);
    assert_eq!(h.fake().places().len(), 1);
}

#[tokio::test]
async fn missing_quote_of_a_held_asset_blocks_trading() {
    let h = Harness::paper(dec!(1000000));
    h.store
        .lock()
        .expect("lock")
        .save_paper_account(&moon_store::PaperAccount {
            cash_ars: dec!(1000000),
            positions: BTreeMap::from([("AAA".to_string(), dec!(10))]),
        })
        .expect("account");
    h.quotes.remove("AAA", Market::Bcba);
    assert!(h.engine.run_monthly(TODAY).await.is_err());
    assert!(all_orders(&h, ExecMode::Paper).is_empty());
}

#[tokio::test]
async fn usd_quote_for_bcba_asset_is_error() {
    let h = Harness::paper(dec!(1000000));
    h.quotes.set("AAA", Market::Bcba, dec!(10), Currency::Usd);
    let err = h.engine.valuation().await.expect_err("must fail");
    assert!(err.to_string().contains("moneda"), "{err}");
    assert!(h.engine.run_monthly(TODAY).await.is_err());
    assert!(all_orders(&h, ExecMode::Paper).is_empty());
}

#[tokio::test]
async fn idempotent_batch_places_once() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    let v = h.engine.valuation().await.expect("valuation");
    let as_of = previous_trading_day(TODAY, &h.engine.config().schedule.holidays);
    let targets = h.engine.decide(as_of).expect("decision").final_weights;
    let plan = h.engine.plan(&targets, &v, false).expect("plan");
    let batch = Uuid::new_v4();

    let first = h
        .engine
        .execute(&plan, &targets, OrderReason::Monthly, batch, false)
        .await
        .expect("first");
    assert!(!first.orders.is_empty());
    let placed = h.fake().places().len();
    assert_eq!(placed, first.orders.len());

    let second = h
        .engine
        .execute(&plan, &targets, OrderReason::Monthly, batch, false)
        .await
        .expect("second");
    assert_eq!(h.fake().places().len(), placed, "no order is placed twice");
    let ids = |r: &moon_app::engine::ExecutionReport| {
        let mut v: Vec<_> = r.orders.iter().map(|o| o.id).collect();
        v.sort();
        v
    };
    assert_eq!(ids(&first), ids(&second));
    for o in &first.orders {
        assert_eq!(o.id, order_id_for(batch, &o.asset, o.side));
    }
}

#[tokio::test]
async fn sent_order_not_resent_and_cancelled_at_close() {
    let cash = dec!(1200000);
    let h = Harness::live(TradingMode::LiveAuto, cash);
    let out = h.engine.run_monthly(TODAY).await.expect("monthly");
    let MonthlyOutcome::Executed(report) = out else {
        panic!("expected Executed, got {out:?}");
    };
    assert!(!report.orders.is_empty());
    assert!(report.orders.iter().all(|o| o.status == OrderStatus::Sent));
    let spent: Decimal = h
        .fake()
        .places()
        .iter()
        .map(|t| t.qty * t.limit_price.expect("limit"))
        .sum();
    assert!(
        spent <= cash,
        "buys must fit the broker's real cash: {spent}"
    );

    // Re-running the same batch does not place anything again.
    let placed = h.fake().places().len();
    assert_eq!(placed, report.orders.len());

    h.engine.run_daily_close(TODAY).await.expect("daily close");
    assert_eq!(h.fake().cancels().len(), placed);
    for o in &report.orders {
        let stored = h
            .store
            .lock()
            .expect("lock")
            .get_order(o.id)
            .expect("get")
            .expect("order");
        assert_eq!(stored.status, OrderStatus::Cancelled);
    }
    assert!(
        h.store
            .lock()
            .expect("lock")
            .open_orders(ExecMode::Live)
            .expect("open")
            .is_empty()
    );
}

#[tokio::test]
async fn broker_error_marks_order_unknown_and_never_resends() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_fail_place(true);
    let MonthlyOutcome::Executed(report) = h.engine.run_monthly(TODAY).await.expect("monthly")
    else {
        panic!("expected Executed");
    };
    assert_eq!(
        report.orders.len(),
        1,
        "the batch stops at the first failure"
    );
    let o = &report.orders[0];
    assert_eq!(o.status, OrderStatus::Failed);
    assert!(
        o.error
            .as_deref()
            .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME)),
        "{:?}",
        o.error
    );
    assert!(!report.errors.is_empty());
    assert!(h.fake().places().is_empty());

    // Same batch again, with the broker healthy: the stored record blocks any re-send.
    h.fake().set_fail_place(false);
    let v = h.engine.valuation().await.expect("valuation");
    let targets = h
        .engine
        .decide(previous_trading_day(TODAY, &[]))
        .expect("decision")
        .final_weights;
    let plan = h.engine.plan(&targets, &v, false).expect("plan");
    let again = h
        .engine
        .execute(
            &plan,
            &targets,
            OrderReason::Monthly,
            report.batch_id,
            false,
        )
        .await
        .expect("again");
    assert_eq!(again.orders.len(), 1);
    assert!(h.fake().places().is_empty());
}

#[tokio::test]
async fn paused_monthly_does_nothing() {
    let h = Harness::paper(dec!(1000000));
    h.engine.set_paused(true).expect("pause");
    assert!(h.engine.is_paused().expect("paused"));
    let out = h.engine.run_monthly(TODAY).await.expect("monthly");
    assert_eq!(out, MonthlyOutcome::NoChanges);
    assert!(all_orders(&h, ExecMode::Paper).is_empty());
}

#[tokio::test]
async fn paper_deposit_keeps_nav() {
    let h = Harness::paper(dec!(1200000));
    let v = h.engine.valuation().await.expect("valuation");
    assert_eq!(v.equity_usd, dec!(1000));
    let tracker = NavTracker::start(v.equity_usd);
    let nav_before = tracker.nav(v.equity_usd).expect("nav");
    let nav_key = h.engine.state_key(STATE_NAV);
    h.store
        .lock()
        .expect("lock")
        .set_state(&nav_key, &tracker)
        .expect("state");

    h.engine.paper_deposit(dec!(600000)).await.expect("deposit");

    let v2 = h.engine.valuation().await.expect("valuation");
    assert_eq!(v2.equity_usd, dec!(1500));
    let t2: NavTracker = h
        .store
        .lock()
        .expect("lock")
        .get_state(&nav_key)
        .expect("get")
        .expect("tracker");
    let nav_after = t2.nav(v2.equity_usd).expect("nav");
    assert!((nav_before - nav_after).abs() < 1e-9);
    let flows = h
        .store
        .lock()
        .expect("lock")
        .flows_between(ExecMode::Paper, far_past(), far_future())
        .expect("flows");
    assert_eq!(flows.len(), 1);
    assert_eq!(flows[0].kind, FlowKind::Deposit);

    h.engine.paper_reset().await.expect("reset");
    let v3 = h.engine.valuation().await.expect("valuation");
    assert_eq!(v3.cash_ars, dec!(1000000));
    assert!(
        h.store
            .lock()
            .expect("lock")
            .get_state::<NavTracker>(&nav_key)
            .expect("get")
            .is_none()
    );
}

#[tokio::test]
async fn paper_commands_fail_outside_paper_mode() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1000));
    let err = h
        .engine
        .paper_deposit(dec!(100))
        .await
        .expect_err("deposit");
    assert!(err.to_string().contains("Solo disponible en modo paper"));
    let err = h.engine.paper_reset().await.expect_err("reset");
    assert!(err.to_string().contains("Solo disponible en modo paper"));
}

#[tokio::test]
async fn daily_close_records_snapshot_and_flows() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1800000));
    let nav_key = h.engine.state_key(STATE_NAV);
    h.store
        .lock()
        .expect("lock")
        .set_state(&nav_key, &NavTracker::start(dec!(1000)))
        .expect("state");
    h.fake().add_flow(ExternalFlow {
        external_id: "dep-1".into(),
        kind: FlowKind::Deposit,
        amount_ars: dec!(600000),
        at: clock_time(),
    });

    h.engine.run_daily_close(TODAY).await.expect("close");
    h.engine.run_daily_close(TODAY).await.expect("close again");

    let store = h.store.lock().expect("lock");
    let flows = store
        .flows_between(ExecMode::Live, far_past(), far_future())
        .expect("flows");
    assert_eq!(flows.len(), 1, "the same flow is not recorded twice");
    let tracker: NavTracker = store.get_state(&nav_key).expect("get").expect("tracker");
    assert_eq!(tracker.units, dec!(1500));
    let snap = store
        .latest_snapshot(ExecMode::Live)
        .expect("snapshot")
        .expect("exists");
    assert!((snap.nav - 1.0).abs() < 1e-9, "nav {}", snap.nav);
    assert_eq!(snap.equity_usd, dec!(1500));
    assert_eq!(snap.units, dec!(1500));
    assert_eq!(snap.cash_total_ars, Some(dec!(1800000)));
    drop(store);
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("Depósito detectado"))
    );
}

#[tokio::test]
async fn daily_close_warns_about_reconciliation_mismatch() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.engine.run_daily_close(TODAY).await.expect("first close");
    // Positions appear that no order explains.
    h.clock.advance(Duration::days(1));
    h.fake().set_position("AAA", dec!(5));
    h.engine
        .run_daily_close(TODAY + Duration::days(1))
        .await
        .expect("second close");
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("Conciliación") && m.contains("AAA")),
        "{:?}",
        h.notifier.messages()
    );
}

#[tokio::test]
async fn status_and_weekly_texts_are_spanish() {
    let h = Harness::paper(dec!(1000000));
    h.engine.run_monthly(TODAY).await.expect("monthly");
    let status = h.engine.status_text().await.expect("status");
    assert!(status.contains("Estado"), "{status}");
    assert!(status.contains("Patrimonio"), "{status}");
    assert!(status.contains("Última decisión"), "{status}");
    let weekly = h.engine.weekly_report().await.expect("weekly");
    assert!(weekly.contains("Resumen semanal"), "{weekly}");
    assert!(weekly.contains("comisiones"), "{weekly}");
}

#[test]
fn engine_rejects_live_without_opt_in() {
    let mut cfg = test_config(TradingMode::LiveAuto);
    cfg.live.enabled = false;
    assert!(try_engine(cfg).is_err());
    // paper config over a live broker is rejected as well
    assert!(try_engine(test_config(TradingMode::Paper)).is_err());
}

#[test]
fn previous_trading_day_skips_weekends_and_holidays() {
    let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).expect("date");
    // Monday -> previous Friday
    assert_eq!(previous_trading_day(d(2026, 11, 23), &[]), d(2026, 11, 20));
    // Tuesday -> Monday, unless Monday is a holiday
    assert_eq!(previous_trading_day(d(2026, 11, 24), &[]), d(2026, 11, 23));
    assert_eq!(
        previous_trading_day(d(2026, 11, 24), &[d(2026, 11, 23)]),
        d(2026, 11, 20)
    );
}

// --- fix round 1: reliable emergency exits, sellable caps, intraday flows ---

/// Holds 100 AAA at 10,000 ARS (8.33 USD at CCL 1200) while the risk index last saw them at
/// 10 USD with a peak of 1.0: the next risk check sees a 16.7 % market fall and trips the breaker.
fn arm_breaker(h: &Harness) {
    h.fake().set_position("AAA", dec!(100));
    set_risk_state(h, dec!(10));
}

fn set_risk_state(h: &Harness, prev_price_usd: Decimal) {
    let index = RiskIndex {
        level: 1.0,
        holdings: BTreeMap::from([("AAA".to_string(), dec!(100))]),
        prices_usd: BTreeMap::from([("AAA".to_string(), prev_price_usd)]),
        at: Some(clock_time()),
        ..RiskIndex::default()
    };
    let (index_key, dd_key) = (
        h.engine.state_key(STATE_RISK_INDEX),
        h.engine.state_key(STATE_DRAWDOWN),
    );
    let mut s = h.store.lock().expect("lock");
    s.set_state(&index_key, &index).expect("index");
    s.set_state(
        &dd_key,
        &DrawdownState {
            peak_nav: 1.0,
            // These older tests exercise the emergency path itself: one prior breach means the
            // next breaching check already confirms the trip.
            breaches: 1,
            ..DrawdownState::default()
        },
    )
    .expect("drawdown");
}

fn emergency_orders(h: &Harness) -> Vec<moon_store::OrderRecord> {
    all_orders(h, ExecMode::Live)
        .into_iter()
        .filter(|o| o.reason == OrderReason::Emergency)
        .collect()
}

#[tokio::test]
async fn rejected_emergency_sell_is_retried_with_a_new_batch() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(0));
    arm_breaker(&h);
    h.fake().set_reject_place(true);
    let first = h.engine.run_risk_check().await.expect("first check");
    assert!(first.tripped);
    let rep = first.emergency.expect("emergency attempted");
    assert_eq!(rep.orders[0].status, OrderStatus::Rejected);
    assert!(h.fake().places().is_empty());

    h.fake().set_reject_place(false);
    h.fake().set_fill_on_place(Some(Some(dec!(10))));
    let second = h.engine.run_risk_check().await.expect("second check");
    assert!(!second.tripped, "the trip itself is reported once");
    let rep = second.emergency.expect("retried");
    assert_eq!(rep.orders[0].status, OrderStatus::Filled);
    let places = h.fake().places();
    assert_eq!(places.len(), 1);
    assert_eq!(places[0].side, Side::Sell);
    // The first attempt was a rejected limit order: the retry goes at market.
    assert_eq!(places[0].order_type, moon_core::types::OrderType::Market);
    assert_eq!(places[0].limit_price, None);

    let orders = emergency_orders(&h);
    assert_eq!(orders.len(), 2);
    assert_ne!(orders[0].batch_id, orders[1].batch_id);

    // Everything is sold: the next check has nothing to do.
    h.engine.run_risk_check().await.expect("third check");
    assert_eq!(h.fake().places().len(), 1);
}

#[tokio::test]
async fn emergency_unknown_outcome_is_never_resent() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(0));
    arm_breaker(&h);
    h.fake().set_fail_place(true);
    let first = h.engine.run_risk_check().await.expect("first check");
    let rep = first.emergency.expect("attempted");
    assert_eq!(rep.orders[0].status, OrderStatus::Failed);
    h.fake().set_fail_place(false);

    let marker = "resultado desconocido y NO se reenvía";
    for _ in 0..3 {
        h.engine.run_risk_check().await.expect("check");
    }
    assert!(h.fake().places().is_empty(), "never re-sent");
    let warned: Vec<String> = h
        .notifier
        .messages()
        .into_iter()
        .filter(|m| m.contains(marker))
        .collect();
    assert_eq!(warned.len(), 1, "warned once per day: {warned:?}");
    assert!(warned[0].contains("IOL"));
}

#[tokio::test]
async fn emergency_sell_left_sent_is_not_duplicated() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(0));
    arm_breaker(&h);
    for _ in 0..3 {
        h.engine.run_risk_check().await.expect("check");
    }
    assert_eq!(h.fake().places().len(), 1);
    assert_eq!(emergency_orders(&h).len(), 1);
}

#[tokio::test]
async fn non_cash_refuge_is_bought_after_the_emergency_sells() {
    let h = Harness::live_with(TradingMode::LiveAuto, dec!(0), |cfg| {
        cfg.assets = vec![
            moon_core::testutil::spec("AAA", moon_core::asset::Role::Risky),
            moon_core::testutil::spec("BBB", moon_core::asset::Role::Risky),
            moon_core::testutil::spec("REF", moon_core::asset::Role::Refuge),
        ];
    });
    arm_breaker(&h);
    h.fake().set_fill_on_place(Some(Some(dec!(0))));
    let out = h.engine.run_risk_check().await.expect("check");
    assert!(out.tripped);
    let places = h.fake().places();
    assert_eq!(places.len(), 2, "{places:?}");
    assert_eq!(
        (places[0].symbol.as_str(), places[0].side),
        ("AAA", Side::Sell)
    );
    assert_eq!(
        (places[1].symbol.as_str(), places[1].side),
        ("REF", Side::Buy)
    );
    assert!(places[1].qty > Decimal::ZERO);

    h.engine.run_risk_check().await.expect("again");
    assert_eq!(
        h.fake().places().len(),
        2,
        "the refuge is bought once per trip"
    );
}

#[tokio::test]
async fn emergency_sell_is_capped_at_sellable() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.fake().set_sellable("AAA", dec!(40));
    h.engine.run_risk_check().await.expect("check");
    let places = h.fake().places();
    assert_eq!(places.len(), 1);
    assert_eq!(places[0].qty, dec!(40));

    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.fake().set_sellable("AAA", dec!(0));
    h.engine.run_risk_check().await.expect("check");
    assert!(h.fake().places().is_empty());
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("cantidad vendible")),
        "{:?}",
        h.notifier.messages()
    );
}

#[tokio::test]
async fn risk_check_fails_closed_on_quote_outage() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_position("AAA", dec!(10));
    h.quotes.remove("AAA", Market::Bcba);
    let err = h.engine.run_risk_check().await.expect_err("must fail");
    assert!(
        err.to_string().contains("control de caída no corrió"),
        "{err}"
    );
}

#[tokio::test]
async fn cash_movements_never_trip_the_breaker() {
    // Holds 100 AAA; the index saw them at exactly today's USD price.
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_position("AAA", dec!(100));
    set_risk_state(&h, dec!(10000) / dec!(1200));
    let out = h.engine.run_risk_check().await.expect("first check");
    assert!(!out.tripped);
    assert!((out.level - 1.0).abs() < 1e-9, "level {}", out.level);

    // A withdrawal of almost everything, a pending buy (cash committed, total unchanged) and a
    // peso devaluation (all cash lost value): none of it is a market move of what is held.
    h.fake().set_cash(dec!(0), dec!(0));
    let out = h.engine.run_risk_check().await.expect("withdrawal");
    assert!(!out.tripped);
    h.fake().set_cash(dec!(0), dec!(1200000));
    let out = h.engine.run_risk_check().await.expect("committed");
    assert!(!out.tripped);
    assert!((out.level - 1.0).abs() < 1e-9, "level {}", out.level);
    assert!(out.drawdown < 1e-9);
    assert!(h.fake().places().is_empty());
}

#[tokio::test]
async fn equity_uses_total_cash_including_committed_funds() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_cash(dec!(0), dec!(1200000));
    let v = h.engine.valuation().await.expect("valuation");
    assert_eq!(v.cash_ars, dec!(0));
    assert_eq!(v.cash_total_ars, dec!(1200000));
    assert_eq!(v.equity_ars, dec!(1200000));
    assert_eq!(v.equity_usd, dec!(1000));
}

#[tokio::test]
async fn paper_state_does_not_leak_into_live() {
    // A paper run left a high peak and a tripped breaker under its own keys, and the old
    // global keys are still there: a live start must ignore all of it.
    let h = Harness::live(TradingMode::LiveAuto, dec!(10000));
    {
        let mut s = h.store.lock().expect("lock");
        let paper_dd = DrawdownState {
            peak_nav: 50.0,
            tripped: true,
            tripped_on: Some(TODAY),
            breaches: 0,
        };
        s.set_state("drawdown:paper", &paper_dd).expect("paper dd");
        s.set_state("drawdown", &paper_dd).expect("old global key");
        s.set_state(
            "risk_index:paper",
            &RiskIndex {
                level: 0.2,
                ..RiskIndex::new()
            },
        )
        .expect("paper index");
    }
    h.fake().set_position("AAA", dec!(1));
    let out = h.engine.run_risk_check().await.expect("live check");
    assert!(!out.tripped);
    assert!(out.emergency.is_none());
    assert!((out.level - 1.0).abs() < 1e-9, "level {}", out.level);
    assert!(h.fake().places().is_empty());
    let s = h.store.lock().expect("lock");
    let live_dd: DrawdownState = s.get_state("drawdown:live").expect("get").expect("live dd");
    assert!(!live_dd.tripped);
    assert!((live_dd.peak_nav - 1.0).abs() < 1e-9);
    let paper_dd: DrawdownState = s.get_state("drawdown:paper").expect("get").expect("paper");
    assert!(paper_dd.tripped, "paper state is untouched");
}

#[tokio::test]
async fn monthly_after_a_trip_rebases_the_peak_and_clears_the_breaker() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    arm_breaker(&h);
    let out = h.engine.run_risk_check().await.expect("trip");
    assert!(out.tripped);
    h.engine.run_monthly(TODAY).await.expect("monthly");
    let dd_key = h.engine.state_key(STATE_DRAWDOWN);
    let dd: DrawdownState = h
        .store
        .lock()
        .expect("lock")
        .get_state(&dd_key)
        .expect("get")
        .expect("dd");
    assert!(!dd.tripped);
    assert!(
        dd.peak_nav < 1.0,
        "peak re-based at the current level: {dd:?}"
    );
}

#[tokio::test]
async fn monthly_without_a_trip_leaves_the_peak_alone() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    set_risk_state(&h, dec!(10000) / dec!(1200));
    h.fake().set_position("AAA", dec!(100));
    // Peak 1.0, level 1.0: raise the peak by hand and check the monthly does not touch it.
    let dd_key = h.engine.state_key(STATE_DRAWDOWN);
    h.store
        .lock()
        .expect("lock")
        .set_state(
            &dd_key,
            &DrawdownState {
                peak_nav: 1.3,
                ..DrawdownState::default()
            },
        )
        .expect("dd");
    h.engine.run_monthly(TODAY).await.expect("monthly");
    let dd: DrawdownState = h
        .store
        .lock()
        .expect("lock")
        .get_state(&dd_key)
        .expect("get")
        .expect("dd");
    assert!((dd.peak_nav - 1.3).abs() < 1e-12, "{dd:?}");
}

#[tokio::test]
async fn failed_monthly_does_not_rearm_the_breaker() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    // Every order of the monthly is rejected: nothing was accepted, so the breaker stays latched.
    h.fake().set_reject_place(true);
    let out = h.engine.run_monthly(TODAY).await.expect("monthly");
    assert!(!out.is_complete(), "{out:?}");
    let dd_key = h.engine.state_key(STATE_DRAWDOWN);
    let dd: DrawdownState = h
        .store
        .lock()
        .expect("lock")
        .get_state(&dd_key)
        .expect("get")
        .expect("dd");
    assert!(dd.tripped);
}

#[tokio::test]
async fn emergency_sweep_works_without_quotes() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    // Trip first, then lose every quote: the retry must still sell, at market.
    h.fake().set_reject_place(true);
    let out = h.engine.run_risk_check().await.expect("trip");
    assert!(out.tripped);
    h.fake().set_reject_place(false);
    for asset in ["AAA", "BBB"] {
        h.quotes.remove(asset, Market::Bcba);
    }
    let out = h
        .engine
        .run_risk_check()
        .await
        .expect("retry without quotes");
    let rep = out.emergency.expect("retried");
    assert_eq!(rep.orders[0].status, OrderStatus::Sent);
    let places = h.fake().places();
    assert_eq!(places.len(), 1);
    assert_eq!(places[0].side, Side::Sell);
    assert_eq!(places[0].qty, dec!(100));
    assert_eq!(places[0].order_type, moon_core::types::OrderType::Market);
    assert_eq!(places[0].limit_price, None);
}

#[tokio::test]
async fn trip_cancels_open_normal_orders_before_selling() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    let MonthlyOutcome::Executed(rep) = h.engine.run_monthly(TODAY).await.expect("monthly") else {
        panic!("expected Executed");
    };
    assert!(!rep.orders.is_empty());
    let normal_ids: Vec<Uuid> = rep.orders.iter().map(|o| o.id).collect();
    assert!(h.fake().cancels().is_empty());

    arm_breaker(&h);
    let out = h.engine.run_risk_check().await.expect("trip");
    assert!(out.tripped);
    assert_eq!(h.fake().cancels().len(), normal_ids.len());
    for id in normal_ids {
        let o = h
            .store
            .lock()
            .expect("lock")
            .get_order(id)
            .expect("get")
            .expect("order");
        assert_eq!(o.status, OrderStatus::Cancelled);
    }
    // The emergency sell itself is not cancelled.
    assert_eq!(emergency_orders(&h).len(), 1);
    assert_eq!(emergency_orders(&h)[0].status, OrderStatus::Sent);
}

#[tokio::test]
async fn missing_quote_of_a_not_held_asset_only_skips_that_buy() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.quotes.remove("AAA", Market::Bcba);
    let MonthlyOutcome::Executed(rep) = h.engine.run_monthly(TODAY).await.expect("monthly") else {
        panic!("expected Executed");
    };
    assert!(!rep.orders.is_empty());
    assert!(rep.orders.iter().all(|o| o.symbol == "BBB"));
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("Sin cotización de AAA")),
        "{:?}",
        h.notifier.messages()
    );
    // The risk check does not need AAA either.
    h.engine.run_risk_check().await.expect("risk check");
}

#[tokio::test]
async fn buy_cash_check_uses_the_limit_price_and_fees() {
    // No cash buffer, no slippage, 5 % limit tolerance: the plan fits the cash at the reference
    // price, but at the limit price only one of the two buys can be paid.
    let h = Harness::live_with(TradingMode::LiveAuto, dec!(1200000), |cfg| {
        cfg.risk.cash_buffer = 0.0;
        cfg.execution.slippage_rate = dec!(0);
        cfg.execution.limit_tolerance = dec!(0.05);
    });
    let MonthlyOutcome::Executed(rep) = h.engine.run_monthly(TODAY).await.expect("monthly") else {
        panic!("expected Executed");
    };
    let fees = &h.engine.config().fees;
    let spent: Decimal = h
        .fake()
        .places()
        .iter()
        .map(|t| {
            let v = t.qty * t.limit_price.expect("limit");
            v + fees.total(v)
        })
        .sum();
    assert!(spent <= dec!(1200000), "spent {spent}");
    assert_eq!(h.fake().places().len(), 1, "{:?}", rep.errors);
    assert!(
        rep.errors
            .iter()
            .any(|e| e.contains("efectivo insuficiente")),
        "{:?}",
        rep.errors
    );
}

#[tokio::test]
async fn definite_rejection_is_recorded_as_rejected_not_unknown() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_reject_place(true);
    let MonthlyOutcome::Executed(rep) = h.engine.run_monthly(TODAY).await.expect("monthly") else {
        panic!("expected Executed");
    };
    assert!(!rep.orders.is_empty());
    for o in &rep.orders {
        assert_eq!(o.status, OrderStatus::Rejected);
        assert!(
            !o.error
                .as_deref()
                .unwrap_or("")
                .starts_with(UNKNOWN_OUTCOME),
            "{:?}",
            o.error
        );
    }
}

#[tokio::test]
async fn max_equity_scales_targets_down() {
    let h = Harness::live_with(TradingMode::LiveAuto, dec!(12000000), |cfg| {
        cfg.risk.max_equity_usd = Some(dec!(1000));
    });
    let v = h.engine.valuation().await.expect("valuation");
    assert_eq!(v.equity_usd, dec!(10000));
    let as_of = previous_trading_day(TODAY, &[]);
    let targets = h.engine.decide(as_of).expect("decision").final_weights;
    let plan = h.engine.plan(&targets, &v, false).expect("plan");
    let bought: Decimal = plan.buys().iter().map(|o| o.est_value).sum();
    assert!(bought > Decimal::ZERO);
    assert!(
        bought <= dec!(1200000),
        "only 1000 USD are managed: {bought}"
    );
}

#[tokio::test]
async fn estimated_fees_are_flagged() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_fill_on_place(Some(None));
    let MonthlyOutcome::Executed(rep) = h.engine.run_monthly(TODAY).await.expect("monthly") else {
        panic!("expected Executed");
    };
    assert!(!rep.orders.is_empty());
    for o in &rep.orders {
        assert_eq!(o.status, OrderStatus::Filled);
        assert!(o.fees_estimated);
        let avg = o.avg_fill_price.expect("avg");
        let expected = h
            .engine
            .config()
            .fees
            .total(moon_core::money::round_money(o.filled_qty * avg));
        assert_eq!(o.fees, expected);
    }
}

#[tokio::test]
async fn limit_prices_follow_the_tick() {
    use moon_core::asset::AssetId;
    use moon_core::rebalance::{PlannedOrder, RebalancePlan};
    use moon_core::weights::TargetWeights;

    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.quotes
        .set("AAA", Market::Bcba, dec!(10001.13), Currency::Ars);
    h.quotes
        .set("BBB", Market::Bcba, dec!(10001.13), Currency::Ars);
    h.engine.run_monthly(TODAY).await.expect("monthly");
    let buys = h.fake().places();
    assert!(!buys.is_empty());
    for t in &buys {
        // 10001.13 * 1.005 = 10051.13565, rounded up to the 0.01 tick
        assert_eq!(t.limit_price, Some(dec!(10051.14)));
    }

    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.quotes
        .set("AAA", Market::Bcba, dec!(10001.13), Currency::Ars);
    h.fake().set_position("AAA", dec!(10));
    let v = h.engine.valuation().await.expect("valuation");
    let plan = RebalancePlan {
        equity: v.equity_ars,
        orders: vec![PlannedOrder {
            asset: AssetId::new("AAA"),
            side: Side::Sell,
            qty: dec!(10),
            ref_price: dec!(10001.13),
            exec_price: dec!(9971.1261),
            est_value: dec!(99711.26),
            est_fees: dec!(0),
        }],
        skipped: vec![],
        est_cash_after: dec!(0),
    };
    let targets = TargetWeights::all_in(AssetId::cash());
    h.engine
        .execute(&plan, &targets, OrderReason::Monthly, Uuid::new_v4(), false)
        .await
        .expect("execute");
    let sells: Vec<_> = h
        .fake()
        .places()
        .into_iter()
        .filter(|t| t.side == Side::Sell)
        .collect();
    assert_eq!(sells.len(), 1);
    // 10001.13 * 0.995 = 9951.12435, rounded down to the tick
    assert_eq!(sells[0].limit_price, Some(dec!(9951.12)));
}

#[tokio::test]
async fn oversize_order_is_split_into_tranches() {
    // 100 USD at CCL 1200 = 120,000 ARS per order = 12 units at 10,000 ARS; each buy is larger.
    let h = Harness::live_with(TradingMode::LiveAuto, dec!(1200000), |cfg| {
        cfg.risk.max_order_usd = dec!(100);
    });
    let out = h.engine.run_monthly(TODAY).await.expect("monthly");
    let MonthlyOutcome::Executed(report) = out else {
        panic!("expected Executed, got {out:?}");
    };
    let places = h.fake().places();
    assert!(places.len() > 2, "each asset needs several tranches");
    for t in &places {
        let value = t.qty * t.limit_price.expect("limit");
        // limit price is slightly above the 10,000 reference (tolerance), so allow that margin
        assert!(t.qty <= dec!(12), "tranche of {} units", t.qty);
        assert!(value <= dec!(125000), "{value}");
    }
    // distinct idempotency keys for every tranche
    let mut keys: Vec<_> = places.iter().map(|t| t.idempotency_key).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), places.len());
    assert_eq!(report.orders.len(), places.len());
}

#[tokio::test]
async fn tranches_count_toward_the_daily_order_limit() {
    let h = Harness::live_with(TradingMode::LiveAuto, dec!(1200000), |cfg| {
        cfg.risk.max_order_usd = dec!(100);
        cfg.risk.max_orders_per_day = 3;
    });
    let out = h.engine.run_monthly(TODAY).await;
    // Either the first phase or the recomputed buys hit the limit: nothing is sent.
    let blocked = match &out {
        Err(e) => e.to_string().contains("límites de riesgo"),
        Ok(MonthlyOutcome::Executed(r)) => {
            r.errors.iter().any(|e| e.contains("límites de riesgo")) || r.orders.is_empty()
        }
        Ok(_) => false,
    };
    assert!(blocked, "{out:?}");
    assert!(h.fake().places().len() <= 3, "{}", h.fake().places().len());
}

// --- fix round 2 ---

#[tokio::test]
async fn daily_close_survives_flow_query_failure() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_fail_flows(true);
    h.engine.run_daily_close(TODAY).await.expect("close");
    let snap = h
        .store
        .lock()
        .expect("lock")
        .latest_snapshot(ExecMode::Live)
        .expect("snapshot");
    assert!(snap.is_some(), "the snapshot is still written");
    let warned = h
        .notifier
        .messages()
        .into_iter()
        .filter(|m| m.contains("No se pudieron consultar depósitos/extracciones"))
        .count();
    assert_eq!(warned, 1);
}

#[tokio::test]
async fn daily_close_reports_unexplained_cash_difference() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.engine.run_daily_close(TODAY).await.expect("first close");
    h.clock.advance(Duration::days(1));
    // 100,000 ARS vanish with no order or flow behind them.
    h.fake().set_cash(dec!(1100000), dec!(1100000));
    h.engine
        .run_daily_close(TODAY + Duration::days(1))
        .await
        .expect("second close");
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("diferencia de caja") && m.contains("esperado 1200000")),
        "{:?}",
        h.notifier.messages()
    );
}

#[tokio::test]
async fn daily_close_cash_and_positions_reconcile_across_days() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_fill_on_place(Some(Some(dec!(10))));
    h.engine.run_daily_close(TODAY).await.expect("first close");
    // Day 2: the monthly buys (own fills); no close that day.
    h.clock.advance(Duration::days(1));
    h.engine
        .run_monthly(TODAY + Duration::days(1))
        .await
        .expect("monthly");
    // Day 3: the close must account for the fills of day 2, not only of day 3.
    h.clock.advance(Duration::days(1));
    h.engine
        .run_daily_close(TODAY + Duration::days(2))
        .await
        .expect("third close");
    let bad: Vec<String> = h
        .notifier
        .messages()
        .into_iter()
        .filter(|m| m.contains("Conciliación") || m.contains("diferencia de caja"))
        .collect();
    assert!(bad.is_empty(), "{bad:?}");
}

fn refuge_harness() -> Harness {
    let h = Harness::live_with(TradingMode::LiveAuto, dec!(0), |cfg| {
        cfg.assets = vec![
            moon_core::testutil::spec("AAA", moon_core::asset::Role::Risky),
            moon_core::testutil::spec("BBB", moon_core::asset::Role::Risky),
            moon_core::testutil::spec("REF", moon_core::asset::Role::Refuge),
        ];
    });
    arm_breaker(&h);
    h.fake().set_fill_on_place(Some(Some(dec!(0))));
    h
}

#[tokio::test]
async fn refuge_buy_with_unknown_outcome_is_never_resent() {
    let h = refuge_harness();
    h.fake().set_fail_buys(true);
    h.engine.run_risk_check().await.expect("first check");
    assert_eq!(h.fake().places().len(), 1, "only the sell went through");

    h.fake().set_fail_buys(false);
    for _ in 0..2 {
        h.engine.run_risk_check().await.expect("next check");
    }
    assert_eq!(h.fake().places().len(), 1, "the refuge buy is not re-sent");
    let notices = h
        .notifier
        .messages()
        .into_iter()
        .filter(|m| m.contains("compra del refugio tiene resultado desconocido"))
        .count();
    assert_eq!(notices, 1);
}

#[tokio::test]
async fn rejected_refuge_buy_is_retried() {
    let h = refuge_harness();
    h.fake().set_reject_buys(true);
    h.engine.run_risk_check().await.expect("first check");
    assert_eq!(h.fake().places().len(), 1, "only the sell went through");

    h.fake().set_reject_buys(false);
    h.engine.run_risk_check().await.expect("second check");
    let places = h.fake().places();
    assert_eq!(places.len(), 2, "{places:?}");
    assert_eq!(places[1].symbol, "REF");

    h.engine.run_risk_check().await.expect("third check");
    assert_eq!(h.fake().places().len(), 2);
}

// --- live readiness P1: repricing, fast refuge, refuge without quote ---

use moon_core::types::OrderType;

#[tokio::test]
async fn fresh_quote_gives_an_aggressive_limit_on_the_first_attempt() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("check");
    let places = h.fake().places();
    assert_eq!(places.len(), 1);
    assert_eq!(places[0].order_type, OrderType::Limit);
    assert_eq!(places[0].limit_price, Some(dec!(9700)));
}

#[tokio::test]
async fn stale_quote_sends_the_first_emergency_sell_at_market() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.quotes
        .set_at("AAA", Market::Bcba, clock_time() - Duration::minutes(45));
    h.engine.run_risk_check().await.expect("check");
    let places = h.fake().places();
    assert_eq!(places.len(), 1);
    assert_eq!(places[0].order_type, OrderType::Market);
    assert_eq!(places[0].limit_price, None);
}

#[tokio::test]
async fn quote_from_a_previous_day_is_stale() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    // 00:10 ART; the quote is 20 minutes old but from 23:50 ART of the day before.
    let now = Utc
        .with_ymd_and_hms(2026, 11, 23, 3, 10, 0)
        .single()
        .expect("t");
    h.clock.set(now);
    h.quotes
        .set_at("AAA", Market::Bcba, now - Duration::minutes(20));
    h.engine.run_risk_check().await.expect("check");
    assert_eq!(h.fake().places()[0].order_type, OrderType::Market);
}

#[tokio::test]
async fn unfilled_limit_older_than_the_window_is_cancelled_and_resent_at_market() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    assert_eq!(h.fake().places()[0].order_type, OrderType::Limit);

    h.clock.advance(Duration::minutes(11));
    h.engine.run_risk_check().await.expect("reprice");
    assert_eq!(h.fake().cancels().len(), 1);
    let places = h.fake().places();
    assert_eq!(places.len(), 2, "{places:?}");
    assert_eq!(places[1].side, Side::Sell);
    assert_eq!(places[1].qty, dec!(100));
    assert_eq!(places[1].order_type, OrderType::Market);
    assert_eq!(places[1].limit_price, None);
    let orders = emergency_orders(&h);
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].status, OrderStatus::Cancelled);
    assert_eq!(orders[1].status, OrderStatus::Sent);
    assert_ne!(orders[0].batch_id, orders[1].batch_id);

    // The market order is not cancelled and re-sent again, however old it gets.
    h.clock.advance(Duration::minutes(30));
    h.engine.run_risk_check().await.expect("later");
    assert_eq!(h.fake().places().len(), 2);
    assert_eq!(h.fake().cancels().len(), 1);
}

#[tokio::test]
async fn working_emergency_order_younger_than_the_window_is_left_alone() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    h.clock.advance(Duration::minutes(9));
    h.engine.run_risk_check().await.expect("check");
    assert!(h.fake().cancels().is_empty());
    assert_eq!(h.fake().places().len(), 1);
    assert_eq!(emergency_orders(&h)[0].status, OrderStatus::Sent);
}

#[tokio::test]
async fn an_emergency_order_without_broker_id_is_never_duplicated_by_repricing() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    {
        let mut s = h.store.lock().expect("lock");
        let mut o = s
            .orders_between(ExecMode::Live, far_past(), far_future())
            .expect("orders")
            .remove(0);
        o.broker_order_id = None;
        o.status = OrderStatus::PendingSend;
        s.update_order(&o).expect("update");
    }
    h.clock.advance(Duration::minutes(20));
    h.engine.run_risk_check().await.expect("check");
    assert_eq!(h.fake().places().len(), 1);
    assert!(h.fake().cancels().is_empty());
}

fn refuge_harness_with(
    wait_seconds: u32,
    tweak: impl FnOnce(&mut moon_app::config::AppConfig),
) -> Harness {
    let h = Harness::live_with(TradingMode::LiveAuto, dec!(0), |cfg| {
        cfg.assets = vec![
            moon_core::testutil::spec("AAA", moon_core::asset::Role::Risky),
            moon_core::testutil::spec("BBB", moon_core::asset::Role::Risky),
            moon_core::testutil::spec("REF", moon_core::asset::Role::Refuge),
        ];
        cfg.execution.emergency_fill_wait_seconds = wait_seconds;
        tweak(cfg);
    });
    arm_breaker(&h);
    h
}

#[tokio::test]
async fn paper_emergency_buys_the_refuge_in_the_same_check() {
    let h = Harness::paper_with(dec!(0), |cfg| {
        cfg.assets = vec![
            moon_core::testutil::spec("AAA", moon_core::asset::Role::Risky),
            moon_core::testutil::spec("BBB", moon_core::asset::Role::Risky),
            moon_core::testutil::spec("REF", moon_core::asset::Role::Refuge),
        ];
    });
    h.store
        .lock()
        .expect("lock")
        .save_paper_account(&moon_store::PaperAccount {
            cash_ars: dec!(0),
            positions: BTreeMap::from([("AAA".to_string(), dec!(100))]),
        })
        .expect("account");
    set_risk_state(&h, dec!(10));
    let out = h.engine.run_risk_check().await.expect("check");
    assert!(out.tripped);
    let acct = h.store.lock().expect("lock").paper_account().expect("acct");
    assert!(!acct.positions.contains_key("AAA"), "{:?}", acct.positions);
    assert!(
        acct.positions.get("REF").copied().unwrap_or_default() > Decimal::ZERO,
        "{:?}",
        acct.positions
    );
}

#[tokio::test]
async fn refuge_is_bought_within_the_wait_when_sells_fill_on_refresh() {
    let h = refuge_harness_with(5, |_| {});
    h.fake().set_fill_on_refresh(Some(2));
    h.engine.run_risk_check().await.expect("check");
    let places = h.fake().places();
    assert_eq!(places.len(), 2, "{places:?}");
    assert_eq!(
        (places[0].symbol.as_str(), places[0].side),
        ("AAA", Side::Sell)
    );
    assert_eq!(
        (places[1].symbol.as_str(), places[1].side),
        ("REF", Side::Buy)
    );
}

#[tokio::test]
async fn refuge_waits_only_up_to_the_configured_seconds() {
    // Sells never fill: with a 1 s wait the refuge is not bought in this check.
    let h = refuge_harness_with(1, |_| {});
    let started = std::time::Instant::now();
    h.engine.run_risk_check().await.expect("check");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(h.fake().places().len(), 1, "only the sell");
}

#[tokio::test]
async fn emergency_refuge_buy_is_exempt_from_the_daily_order_limit() {
    let h = refuge_harness_with(0, |cfg| cfg.risk.max_orders_per_day = 1);
    h.fake().set_fill_on_place(Some(Some(dec!(0))));
    h.engine.run_risk_check().await.expect("check");
    let places = h.fake().places();
    assert_eq!(places.len(), 2, "{places:?}");
    assert_eq!(places[1].symbol, "REF");
}

#[tokio::test]
async fn refuge_without_quote_is_an_error_notified_once_per_hour_and_retried() {
    let h = refuge_harness_with(0, |_| {});
    h.fake().set_fill_on_place(Some(Some(dec!(0))));
    h.quotes.remove("REF", Market::Bcba);
    let notices = |h: &Harness| {
        h.notifier
            .messages()
            .iter()
            .filter(|m| m.contains("refugio") && m.contains("REF"))
            .count()
    };
    h.engine.run_risk_check().await.expect("first check");
    assert_eq!(h.fake().places().len(), 1, "only the sell");
    assert_eq!(notices(&h), 1, "{:?}", h.notifier.messages());

    h.engine.run_risk_check().await.expect("second check");
    assert_eq!(notices(&h), 1, "deduplicated within the hour");

    h.clock.advance(Duration::minutes(61));
    h.engine.run_risk_check().await.expect("third check");
    assert_eq!(notices(&h), 2, "notified again after an hour");
    assert_eq!(h.fake().places().len(), 1);

    // The trip is not marked as refuge done: once the quote is back the refuge is bought.
    h.quotes
        .set("REF", Market::Bcba, dec!(10000), Currency::Ars);
    h.engine.run_risk_check().await.expect("fourth check");
    let places = h.fake().places();
    assert_eq!(places.len(), 2, "{places:?}");
    assert_eq!(places[1].symbol, "REF");
}

// --- live fix round F1: confirmed cancels, refuge held, escalation, messages ---

use moon_app::engine::{STATE_EMERGENCY_REFUGE, short_id};
use moon_broker::{Listing, OrderProgress};

fn count_msgs(h: &Harness, needle: &str) -> usize {
    h.notifier
        .messages()
        .iter()
        .filter(|m| m.contains(needle))
        .count()
}

fn progress(status: OrderStatus, filled: Decimal, price: Decimal) -> OrderProgress {
    OrderProgress {
        status,
        filled_qty: filled,
        avg_price: Some(price),
        fees: None,
        listing: Listing::Listed,
    }
}

#[tokio::test]
async fn unconfirmed_cancel_does_not_resend_and_notifies_once_per_hour() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    h.fake().set_cancel_fails(true);
    h.clock.advance(Duration::minutes(11));
    h.engine.run_risk_check().await.expect("reprice");
    assert_eq!(h.fake().cancels().len(), 1);
    assert_eq!(h.fake().places().len(), 1, "no second sell");
    let orders = emergency_orders(&h);
    assert_eq!(orders.len(), 1);
    assert_eq!(
        orders[0].status,
        OrderStatus::Sent,
        "never marked cancelled"
    );
    let marker = "No se pudo cancelar la venta de AAA";
    assert_eq!(count_msgs(&h, marker), 1, "{:?}", h.notifier.messages());
    let notice = h
        .notifier
        .messages()
        .into_iter()
        .find(|m| m.contains(marker))
        .expect("notice");
    assert!(notice.contains(&short_id(orders[0].id)), "{notice}");

    h.clock.advance(Duration::minutes(15));
    h.engine.run_risk_check().await.expect("retry");
    assert_eq!(h.fake().cancels().len(), 2, "the cancel is retried");
    assert_eq!(h.fake().places().len(), 1);
    assert_eq!(count_msgs(&h, marker), 1, "deduplicated within the hour");

    h.clock.advance(Duration::minutes(61));
    h.engine.run_risk_check().await.expect("later");
    assert_eq!(count_msgs(&h, marker), 2);
    assert_eq!(h.fake().places().len(), 1);
}

#[tokio::test]
async fn cancel_accepted_but_not_visible_in_the_refresh_does_not_resend() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    h.fake().set_cancel_confirms(false);
    h.clock.advance(Duration::minutes(11));
    h.engine.run_risk_check().await.expect("reprice");
    assert_eq!(h.fake().cancels().len(), 1);
    assert_eq!(h.fake().places().len(), 1, "{:?}", h.fake().places());
    assert_eq!(emergency_orders(&h)[0].status, OrderStatus::Sent);
    assert_eq!(count_msgs(&h, "no está confirmada"), 1);
}

#[tokio::test]
async fn fill_arriving_with_the_cancel_is_recorded_and_notified_without_a_second_sell() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    // First refresh: still working. IOL refuses the cancel and the second refresh shows the fill.
    h.fake().set_fill_on_refresh(Some(2));
    h.fake().set_cancel_fails(true);
    h.clock.advance(Duration::minutes(11));
    h.engine.run_risk_check().await.expect("reprice");
    assert_eq!(h.fake().places().len(), 1, "no duplicate sell");
    let orders = emergency_orders(&h);
    assert_eq!(orders[0].status, OrderStatus::Filled);
    assert_eq!(orders[0].filled_qty, dec!(100));
    let fills: Vec<String> = h
        .notifier
        .messages()
        .into_iter()
        .filter(|m| m.contains("Venta de emergencia ejecutada"))
        .collect();
    assert_eq!(fills.len(), 1, "{:?}", h.notifier.messages());
    assert!(
        fills[0].contains("AAA") && fills[0].contains("100"),
        "{}",
        fills[0]
    );
    assert!(fills[0].contains(&short_id(orders[0].id)), "{}", fills[0]);
}

#[tokio::test]
async fn confirmed_cancel_resends_only_what_is_left_after_a_partial_fill() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    // 40 of the 100 sold before the cancel: the account holds 60.
    h.fake()
        .set_refresh_result(progress(OrderStatus::PartiallyFilled, dec!(40), dec!(9700)));
    h.fake().set_position("AAA", dec!(60));
    h.clock.advance(Duration::minutes(11));
    h.engine.run_risk_check().await.expect("reprice");
    assert_eq!(h.fake().cancels().len(), 1);
    let places = h.fake().places();
    assert_eq!(places.len(), 2, "{places:?}");
    assert_eq!(places[1].qty, dec!(60));
    assert_eq!(places[1].order_type, OrderType::Market);
    let orders = emergency_orders(&h);
    assert_eq!(orders[0].status, OrderStatus::Cancelled);
    assert_eq!(orders[0].filled_qty, dec!(40));
}

#[tokio::test]
async fn daily_close_never_marks_an_order_cancelled_without_confirmation() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.engine.run_monthly(TODAY).await.expect("monthly");
    assert!(
        all_orders(&h, ExecMode::Live)
            .iter()
            .all(|o| o.status == OrderStatus::Sent)
    );
    h.fake().set_cancel_confirms(false);
    h.engine.run_daily_close(TODAY).await.expect("close");
    assert!(
        all_orders(&h, ExecMode::Live)
            .iter()
            .all(|o| o.status == OrderStatus::Sent),
        "still working at IOL"
    );
    assert!(count_msgs(&h, "no está confirmada") >= 1);
    h.fake().set_cancel_confirms(true);
    h.engine.run_daily_close(TODAY).await.expect("second close");
    assert!(
        all_orders(&h, ExecMode::Live)
            .iter()
            .all(|o| o.status == OrderStatus::Cancelled)
    );
}

fn refuge_done(h: &Harness) -> Option<NaiveDate> {
    let key = h.engine.state_key(STATE_EMERGENCY_REFUGE);
    h.store
        .lock()
        .expect("lock")
        .get_state::<NaiveDate>(&key)
        .expect("state")
}

#[tokio::test]
async fn refuge_buy_that_is_only_sent_is_not_done_and_is_retried_after_the_close() {
    let h = refuge_harness_with(0, |_| {});
    h.fake().set_fill_on_place(Some(Some(dec!(0))));
    h.fake().set_fill_only_sells(true);
    h.engine.run_risk_check().await.expect("trip");
    let places = h.fake().places();
    assert_eq!(places.len(), 2, "{places:?}");
    assert_eq!(places[1].symbol, "REF");
    assert_eq!(refuge_done(&h), None, "sent is not held");

    // Still working at the next check: refreshed, not duplicated.
    h.engine.run_risk_check().await.expect("second check");
    assert_eq!(h.fake().places().len(), 2);
    assert!(
        h.fake().refreshes().iter().any(|(_, s)| s == "REF"),
        "{:?}",
        h.fake().refreshes()
    );
    assert_eq!(refuge_done(&h), None);

    // The close cancels the unfilled buy; the next day the refuge is bought again.
    h.engine.run_daily_close(TODAY).await.expect("close");
    let refuge_orders: Vec<_> = emergency_orders(&h)
        .into_iter()
        .filter(|o| o.symbol == "REF")
        .collect();
    assert_eq!(refuge_orders[0].status, OrderStatus::Cancelled);
    h.clock.advance(Duration::days(1));
    h.engine.run_risk_check().await.expect("next day");
    let places = h.fake().places();
    assert_eq!(places.len(), 3, "{places:?}");
    assert_eq!(places[2].symbol, "REF");
    assert_eq!(places[2].side, Side::Buy);
    assert_eq!(refuge_done(&h), None);
}

#[tokio::test]
async fn refuge_buy_that_fills_marks_the_trip_done() {
    let h = refuge_harness_with(0, |_| {});
    h.fake().set_fill_on_place(Some(Some(dec!(0))));
    h.engine.run_risk_check().await.expect("trip");
    assert_eq!(h.fake().places().len(), 2);
    assert_eq!(refuge_done(&h), Some(TODAY));
    h.engine.run_risk_check().await.expect("next check");
    assert_eq!(h.fake().places().len(), 2);
}

#[tokio::test]
async fn refuge_waits_for_the_sell_proceeds_to_be_credited() {
    let h = refuge_harness_with(0, |_| {});
    h.fake().set_fill_on_place(Some(Some(dec!(0))));
    h.fake().set_cash_lag(true);
    let marker = "esperando acreditación del efectivo de las ventas";
    h.engine.run_risk_check().await.expect("trip");
    assert_eq!(h.fake().places().len(), 1, "only the sell: no cash yet");
    assert_eq!(refuge_done(&h), None, "not done without the refuge");
    assert_eq!(count_msgs(&h, marker), 1, "{:?}", h.notifier.messages());

    h.clock.advance(Duration::minutes(15));
    h.engine.run_risk_check().await.expect("second check");
    assert_eq!(h.fake().places().len(), 1);
    assert_eq!(count_msgs(&h, marker), 1, "once per day");

    h.clock.advance(Duration::days(1));
    h.engine.run_risk_check().await.expect("next day");
    assert_eq!(count_msgs(&h, marker), 2);

    h.fake().release_pending_cash();
    h.clock.advance(Duration::minutes(15));
    h.engine.run_risk_check().await.expect("cash credited");
    let places = h.fake().places();
    assert_eq!(places.len(), 2, "{places:?}");
    assert_eq!(places[1].symbol, "REF");
}

#[tokio::test]
async fn market_rejection_escalates_to_a_deep_limit_on_the_stale_quote() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    // Stale quote: the first attempt is a market order and IOL rejects it for good.
    h.quotes
        .set_at("AAA", Market::Bcba, clock_time() - Duration::minutes(45));
    h.fake().set_reject_type(Some(OrderType::Market));
    h.engine.run_risk_check().await.expect("first check");
    assert!(h.fake().places().is_empty());
    let orders = emergency_orders(&h);
    assert_eq!(orders[0].status, OrderStatus::Rejected);
    assert_eq!(orders[0].limit_price, None);

    h.clock.advance(Duration::minutes(15));
    h.engine.run_risk_check().await.expect("second check");
    let places = h.fake().places();
    assert_eq!(places.len(), 1, "{places:?}");
    assert_eq!(places[0].order_type, OrderType::Limit);
    // 10 % below the last known price (10,000) even though it is stale.
    assert_eq!(places[0].limit_price, Some(dec!(9000)));
    let rejected: Vec<String> = h
        .notifier
        .messages()
        .into_iter()
        .filter(|m| m.contains("fue rechazada por IOL"))
        .collect();
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert!(
        rejected[0].contains("rechazada por el mercado"),
        "{}",
        rejected[0]
    );
    assert!(
        rejected[0].contains(&short_id(orders[0].id)),
        "{}",
        rejected[0]
    );
}

#[tokio::test]
async fn rejected_deep_limit_alternates_back_to_market() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.quotes
        .set_at("AAA", Market::Bcba, clock_time() - Duration::minutes(45));
    h.fake().set_reject_type(Some(OrderType::Market));
    h.engine.run_risk_check().await.expect("market rejected");
    h.fake().set_reject_type(Some(OrderType::Limit));
    h.clock.advance(Duration::minutes(15));
    h.engine
        .run_risk_check()
        .await
        .expect("deep limit rejected");
    assert!(h.fake().places().is_empty());
    h.clock.advance(Duration::minutes(15));
    h.engine.run_risk_check().await.expect("market again");
    let places = h.fake().places();
    assert_eq!(places.len(), 1, "{places:?}");
    assert_eq!(places[0].order_type, OrderType::Market);
    let limits: Vec<Option<Decimal>> = emergency_orders(&h).iter().map(|o| o.limit_price).collect();
    assert_eq!(limits, vec![None, Some(dec!(9000)), None]);
    assert_eq!(count_msgs(&h, "fue rechazada por IOL"), 2);
}

#[tokio::test]
async fn retry_notice_is_deduplicated_once_per_hour() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.fake().set_reject_place(true);
    let marker = "Freno por caída activo: se reintenta la venta de emergencia";
    h.engine.run_risk_check().await.expect("trip");
    h.clock.advance(Duration::minutes(15));
    h.engine.run_risk_check().await.expect("retry 1");
    assert_eq!(count_msgs(&h, marker), 1);
    h.clock.advance(Duration::minutes(15));
    h.engine.run_risk_check().await.expect("retry 2");
    assert_eq!(count_msgs(&h, marker), 1, "deduplicated within the hour");
    h.clock.advance(Duration::minutes(61));
    h.engine.run_risk_check().await.expect("retry 3");
    assert_eq!(count_msgs(&h, marker), 2);
}

#[tokio::test]
async fn refresh_uses_the_persisted_symbol_of_the_order() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    h.clock.advance(Duration::minutes(1));
    h.engine.run_risk_check().await.expect("check");
    assert!(
        h.fake()
            .refreshes()
            .contains(&("fake-1".to_string(), "AAA".to_string())),
        "{:?}",
        h.fake().refreshes()
    );
}

#[tokio::test]
async fn order_missing_from_iol_after_three_refreshes_is_notified_once_per_day() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    h.fake().set_not_listed(Some(Listing::NotListed));
    let id = short_id(emergency_orders(&h)[0].id);
    for _ in 0..2 {
        h.clock.advance(Duration::minutes(1));
        h.engine.run_risk_check().await.expect("check");
    }
    assert_eq!(count_msgs(&h, "no aparece en IOL"), 0);
    h.clock.advance(Duration::minutes(1));
    h.engine.run_risk_check().await.expect("third refresh");
    let notices: Vec<String> = h
        .notifier
        .messages()
        .into_iter()
        .filter(|m| m.contains("no aparece en IOL"))
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains(&id), "{}", notices[0]);
    assert!(notices[0].contains("moon orders resolve"), "{}", notices[0]);
    h.clock.advance(Duration::minutes(1));
    h.engine.run_risk_check().await.expect("fourth refresh");
    assert_eq!(count_msgs(&h, "no aparece en IOL"), 1, "once per day");
}

#[tokio::test]
async fn truncated_list_without_the_order_is_notified_at_once() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    h.fake().set_not_listed(Some(Listing::NotListedTruncated));
    h.clock.advance(Duration::minutes(1));
    h.engine.run_risk_check().await.expect("check");
    assert_eq!(count_msgs(&h, "no aparece en IOL"), 1);
}

#[tokio::test]
async fn order_seen_again_resets_the_missing_counter() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    for listing in [
        Some(Listing::NotListed),
        Some(Listing::NotListed),
        None,
        Some(Listing::NotListed),
        Some(Listing::NotListed),
    ] {
        h.fake().set_not_listed(listing);
        h.clock.advance(Duration::minutes(1));
        h.engine.run_risk_check().await.expect("check");
    }
    assert_eq!(count_msgs(&h, "no aparece en IOL"), 0);
}

#[tokio::test]
async fn orders_that_are_only_sent_are_announced_as_sent() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.engine.run_risk_check().await.expect("trip");
    assert_eq!(
        count_msgs(&h, "Órdenes enviadas"),
        1,
        "{:?}",
        h.notifier.messages()
    );
    assert_eq!(count_msgs(&h, "Ejecución completada"), 0);

    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    arm_breaker(&h);
    h.fake().set_fill_on_place(Some(Some(dec!(0))));
    h.engine.run_risk_check().await.expect("trip");
    assert_eq!(count_msgs(&h, "Ejecución completada"), 1);
}

#[tokio::test]
async fn blocked_assets_notice_names_the_orders_by_short_id() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.engine.run_monthly(TODAY).await.expect("monthly");
    let ids: Vec<String> = all_orders(&h, ExecMode::Live)
        .iter()
        .map(|o| short_id(o.id))
        .collect();
    h.engine.run_monthly(TODAY).await.expect("again");
    let notice = h
        .notifier
        .messages()
        .into_iter()
        .find(|m| m.contains("Se omiten"))
        .expect("notice");
    assert!(ids.iter().any(|id| notice.contains(id)), "{notice}");
}

#[tokio::test]
async fn refuge_unknown_outcome_notice_has_short_id_and_resolve_hint() {
    let h = refuge_harness();
    h.fake().set_fail_buys(true);
    h.engine.run_risk_check().await.expect("check");
    h.fake().set_fail_buys(false);
    h.engine.run_risk_check().await.expect("check");
    let refuge_buy = emergency_orders(&h)
        .into_iter()
        .find(|o| o.symbol == "REF")
        .expect("refuge buy");
    let notice = h
        .notifier
        .messages()
        .into_iter()
        .find(|m| m.contains("compra del refugio tiene resultado desconocido"))
        .expect("notice");
    assert!(notice.contains(&short_id(refuge_buy.id)), "{notice}");
    assert!(notice.contains("moon orders resolve"), "{notice}");
}
