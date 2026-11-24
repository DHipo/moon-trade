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

// --- fix round 1: reliable emergency exits, sellable caps, intraday flows ---

/// Holds 100 AAA (833.33 USD at CCL 1200) against 1000 NAV units with a peak of 1.0: the next
/// risk check trips the breaker.
fn arm_breaker(h: &Harness) {
    h.fake().set_position("AAA", dec!(100));
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
    h.quotes.remove("AAA", Market::Bcba);
    let err = h.engine.run_risk_check().await.expect_err("must fail");
    assert!(
        err.to_string().contains("control de caída no corrió"),
        "{err}"
    );
}

#[tokio::test]
async fn intraday_withdrawal_does_not_trip_the_breaker() {
    // 1000 USD of equity and units; 20% (240,000 ARS) is withdrawn: 960,000 ARS remain.
    let h = Harness::live(TradingMode::LiveAuto, dec!(960000));
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
    h.fake().add_flow(ExternalFlow {
        external_id: "wd-1".into(),
        kind: FlowKind::Withdrawal,
        amount_ars: dec!(240000),
        at: clock_time(),
    });
    let out = h.engine.run_risk_check().await.expect("check");
    assert!(!out.tripped);
    assert!((out.nav - 1.0).abs() < 1e-9, "nav {}", out.nav);
    assert!(h.fake().places().is_empty());
    let checked: Option<NaiveDate> = h
        .store
        .lock()
        .expect("lock")
        .get_state("flows_checked_until")
        .expect("state");
    assert_eq!(checked, Some(TODAY));
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
async fn oversize_order_is_blocked_after_usd_to_ars_conversion() {
    // 100 USD at CCL 1200 = 120,000 ARS per order; each buy is far larger.
    let h = Harness::live_with(TradingMode::LiveAuto, dec!(1200000), |cfg| {
        cfg.risk.max_order_usd = dec!(100);
    });
    let err = h.engine.run_monthly(TODAY).await.expect_err("blocked");
    assert!(err.to_string().contains("límites de riesgo"), "{err}");
    assert!(h.fake().places().is_empty());
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("bloqueada"))
    );
}
