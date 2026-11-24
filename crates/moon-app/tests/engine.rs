mod common;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use common::{Harness, TODAY, clock_time, test_config, try_engine};
use moon_app::config::TradingMode;
use moon_app::engine::{
    MonthlyOutcome, STATE_DRAWDOWN, STATE_NAV, UNKNOWN_OUTCOME, order_id_for, previous_trading_day,
};
use moon_broker::ExternalFlow;
use moon_core::asset::{Currency, Market};
use moon_core::nav::NavTracker;
use moon_core::risk::DrawdownState;
use moon_core::types::{ExecMode, OrderReason, OrderStatus, Side};
use moon_store::{FlowKind, ProposalStatus};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
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
    assert!(!store.recent_decisions(5).expect("decisions").is_empty());
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
    h.fake().set_position("AAA", dec!(100));
    h.engine.set_paused(true).expect("pause");
    // 100 x 10,000 ARS = 833.33 USD at CCL 1200 against 1000 units: NAV 0.833 vs peak 1.0.
    {
        let mut s = h.store.lock().expect("lock");
        s.set_state(STATE_NAV, &NavTracker { units: dec!(1000) })
            .expect("nav");
        s.set_state(
            STATE_DRAWDOWN,
            &DrawdownState {
                peak_nav: 1.0,
                ..DrawdownState::default()
            },
        )
        .expect("drawdown");
    }
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
async fn missing_quote_blocks_trading() {
    let h = Harness::paper(dec!(1000000));
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
    h.store
        .lock()
        .expect("lock")
        .set_state(STATE_NAV, &tracker)
        .expect("state");

    h.engine.paper_deposit(dec!(600000)).await.expect("deposit");

    let v2 = h.engine.valuation().await.expect("valuation");
    assert_eq!(v2.equity_usd, dec!(1500));
    let t2: NavTracker = h
        .store
        .lock()
        .expect("lock")
        .get_state(STATE_NAV)
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
            .get_state::<NavTracker>(STATE_NAV)
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
    h.store
        .lock()
        .expect("lock")
        .set_state(STATE_NAV, &NavTracker::start(dec!(1000)))
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
    let tracker: NavTracker = store.get_state(STATE_NAV).expect("get").expect("tracker");
    assert_eq!(tracker.units, dec!(1500));
    let snap = store
        .latest_snapshot(ExecMode::Live)
        .expect("snapshot")
        .expect("exists");
    assert!((snap.nav - 1.0).abs() < 1e-9, "nav {}", snap.nav);
    assert_eq!(snap.equity_usd, dec!(1500));
    assert_eq!(snap.units, dec!(1500));
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
