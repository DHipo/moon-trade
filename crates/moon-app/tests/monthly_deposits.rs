//! Part B2: monthly batching and idempotency, signal-data guard, deposit-triggered buys,
//! approval recalculation and the service watching its Telegram task.

mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use common::{Harness, TODAY, clock_time};
use moon_app::config::TradingMode;
use moon_app::engine::{
    Engine, MonthlyOutcome, STATE_DEPOSIT_PENDING, STATE_DRAWDOWN, last_day_of_previous_month,
    monthly_batch_id,
};
use moon_app::serve::{run_service, startup_warnings};
use moon_broker::ExternalFlow;
use moon_core::risk::DrawdownState;
use moon_core::types::{ExecMode, OrderReason, OrderStatus, Side};
use moon_store::{FlowKind, OrderRecord, ProposalKind, ProposalStatus};
use rust_decimal_macros::dec;
use uuid::Uuid;

fn far_past() -> DateTime<Utc> {
    clock_time() - ChronoDuration::days(3650)
}

fn far_future() -> DateTime<Utc> {
    clock_time() + ChronoDuration::days(3650)
}

fn orders(h: &Harness, mode: ExecMode) -> Vec<OrderRecord> {
    h.store
        .lock()
        .expect("lock")
        .orders_between(mode, far_past(), far_future())
        .expect("orders")
}

fn set_tripped(h: &Harness) {
    let key = h.engine.state_key(STATE_DRAWDOWN);
    h.store
        .lock()
        .expect("lock")
        .set_state(
            &key,
            &DrawdownState {
                peak_nav: 1.0,
                tripped: true,
                tripped_on: Some(TODAY),
            },
        )
        .expect("state");
}

// --- B2.1: monthly batch ids and idempotency ---

#[tokio::test]
async fn interrupted_monthly_does_not_replace_assets_with_working_orders() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    let first = h.engine.run_monthly(TODAY).await.expect("monthly");
    let MonthlyOutcome::Executed(report) = first else {
        panic!("expected Executed, got {first:?}");
    };
    assert!(!report.orders.is_empty());
    assert!(report.orders.iter().all(|o| o.status == OrderStatus::Sent));
    let placed = h.fake().places().len();
    assert_eq!(
        report.orders[0].batch_id,
        monthly_batch_id(ExecMode::Live, "2026-11", 0)
    );

    // The service restarts and the monthly runs again: the Sent orders are still working.
    let again = h.engine.run_monthly(TODAY).await.expect("monthly again");
    assert_eq!(again, MonthlyOutcome::NoChanges);
    assert_eq!(h.fake().places().len(), placed, "nothing is placed twice");
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("Se omiten") && m.contains("orden pendiente")),
        "{:?}",
        h.notifier.messages()
    );
}

#[tokio::test]
async fn unknown_outcome_order_blocks_its_asset_for_the_month() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_fail_place(true);
    let first = h.engine.run_monthly(TODAY).await.expect("monthly");
    // The first place fails with an unknown outcome and halts the batch: not accepted.
    assert!(!first.is_complete(), "{first:?}");
    let unknown: Vec<_> = orders(&h, ExecMode::Live)
        .into_iter()
        .filter(|o| o.status == OrderStatus::Failed)
        .collect();
    assert_eq!(unknown.len(), 1);

    h.fake().set_fail_place(false);
    let again = h.engine.run_monthly(TODAY).await.expect("monthly again");
    let _ = again;
    let places = h.fake().places();
    assert!(
        places.iter().all(|t| t.symbol != unknown[0].symbol),
        "the asset with an unknown outcome is never re-sent: {places:?}"
    );
}

#[tokio::test]
async fn retry_after_rejections_uses_the_next_attempt_batch() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_reject_place(true);
    let first = h.engine.run_monthly(TODAY).await.expect("monthly");
    assert!(!first.is_complete(), "{first:?}");
    assert!(h.fake().places().is_empty());

    h.fake().set_reject_place(false);
    let second = h.engine.run_monthly(TODAY).await.expect("retry");
    let MonthlyOutcome::Executed(report) = second else {
        panic!("expected Executed, got {second:?}");
    };
    assert!(second_is_complete(&report));
    assert!(
        report
            .orders
            .iter()
            .all(|o| o.batch_id == monthly_batch_id(ExecMode::Live, "2026-11", 1)),
        "the retry runs as attempt 1"
    );
    assert!(!h.fake().places().is_empty());
}

fn second_is_complete(report: &moon_app::engine::ExecutionReport) -> bool {
    MonthlyOutcome::Executed(report.clone()).is_complete()
}

#[test]
fn monthly_batch_id_is_deterministic_per_mode_month_and_attempt() {
    let a = monthly_batch_id(ExecMode::Live, "2026-11", 0);
    assert_eq!(a, monthly_batch_id(ExecMode::Live, "2026-11", 0));
    assert_ne!(a, monthly_batch_id(ExecMode::Paper, "2026-11", 0));
    assert_ne!(a, monthly_batch_id(ExecMode::Live, "2026-12", 0));
    assert_ne!(a, monthly_batch_id(ExecMode::Live, "2026-11", 1));
}

#[test]
fn as_of_is_the_last_calendar_day_of_the_previous_month() {
    let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).expect("date");
    assert_eq!(last_day_of_previous_month(d(2026, 11, 23)), d(2026, 10, 31));
    assert_eq!(last_day_of_previous_month(d(2027, 1, 4)), d(2026, 12, 31));
    assert_eq!(last_day_of_previous_month(d(2028, 3, 1)), d(2028, 2, 29));
}

#[tokio::test]
async fn approval_executes_under_a_batch_derived_from_the_proposal() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    let MonthlyOutcome::ProposalSent(id) = h.engine.run_monthly(TODAY).await.expect("monthly")
    else {
        panic!("expected a proposal");
    };
    h.engine.handle_approval(id, true).await.expect("approve");
    let expected = Uuid::new_v5(&Uuid::NAMESPACE_OID, id.to_string().as_bytes());
    let placed = orders(&h, ExecMode::Live);
    assert!(!placed.is_empty());
    assert!(placed.iter().all(|o| o.batch_id == expected));
}

// --- B2.2: stale signal data aborts the monthly ---

#[tokio::test]
async fn monthly_aborts_when_signal_data_is_stale() {
    let h = Harness::paper(dec!(1000000));
    // Seeded bars end in late December 2026; an `as_of` of 2027-02-28 is far beyond them.
    let later = NaiveDate::from_ymd_opt(2027, 3, 15).expect("date");
    let out = h.engine.run_monthly(later).await.expect("monthly");
    let MonthlyOutcome::Aborted(why) = &out else {
        panic!("expected Aborted, got {out:?}");
    };
    assert!(why.contains("desactualizados"), "{why}");
    assert!(!out.is_complete(), "the monthly stays due");
    assert!(orders(&h, ExecMode::Paper).is_empty());
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("postergado"))
    );
}

// --- B2.4 ---

#[test]
fn monthly_outcome_completion_rules() {
    assert!(MonthlyOutcome::NoChanges.is_complete());
    assert!(MonthlyOutcome::ProposalSent(Uuid::new_v4()).is_complete());
    assert!(!MonthlyOutcome::Aborted("x".into()).is_complete());
}

#[tokio::test]
async fn empty_holiday_calendar_triggers_a_startup_warning() {
    let h = Harness::paper(dec!(1000));
    let warnings = startup_warnings(&h.engine);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("schedule.holidays"), "{warnings:?}");

    let h = Harness::live_with(TradingMode::LiveAuto, dec!(1000), |cfg| {
        cfg.schedule.holidays = vec![NaiveDate::from_ymd_opt(2026, 12, 8).expect("date")];
    });
    assert!(startup_warnings(&h.engine).is_empty());
}

// --- B2.3: deposit-triggered buys ---

fn deposit_orders(h: &Harness, mode: ExecMode) -> Vec<OrderRecord> {
    orders(h, mode)
        .into_iter()
        .filter(|o| o.reason == OrderReason::Deposit)
        .collect()
}

#[tokio::test]
async fn paper_deposit_buys_toward_the_last_monthly_decision() {
    let h = Harness::paper(dec!(1200000));
    h.engine.run_monthly(TODAY).await.expect("monthly");
    let before = orders(&h, ExecMode::Paper).len();
    assert!(deposit_orders(&h, ExecMode::Paper).is_empty());

    h.engine
        .paper_deposit(dec!(1200000))
        .await
        .expect("deposit");
    let dep = deposit_orders(&h, ExecMode::Paper);
    assert!(!dep.is_empty(), "the deposit is invested");
    assert!(dep.iter().all(|o| o.side == Side::Buy));
    assert!(dep.iter().all(|o| o.status == OrderStatus::Filled));
    assert!(orders(&h, ExecMode::Paper).len() > before);
    // nothing is left pending
    let key = h.engine.state_key(STATE_DEPOSIT_PENDING);
    let pending: Option<Vec<String>> = h.store.lock().expect("lock").get_state(&key).expect("get");
    assert!(pending.is_none(), "{pending:?}");
}

#[tokio::test]
async fn deposit_without_a_monthly_decision_buys_nothing() {
    let h = Harness::paper(dec!(1200000));
    h.engine.paper_deposit(dec!(600000)).await.expect("deposit");
    assert!(orders(&h, ExecMode::Paper).is_empty());
}

#[tokio::test]
async fn deposit_while_paused_or_tripped_stays_pending() {
    let h = Harness::paper(dec!(1200000));
    h.engine.run_monthly(TODAY).await.expect("monthly");
    h.engine.set_paused(true).expect("pause");
    h.engine.paper_deposit(dec!(600000)).await.expect("deposit");
    assert!(deposit_orders(&h, ExecMode::Paper).is_empty());
    let key = h.engine.state_key(STATE_DEPOSIT_PENDING);
    let pending: Option<Vec<String>> = h.store.lock().expect("lock").get_state(&key).expect("get");
    assert_eq!(pending.map(|p| p.len()), Some(1));

    h.engine.set_paused(false).expect("resume");
    set_tripped(&h);
    h.engine.check_deposits().await.expect("check");
    assert!(
        deposit_orders(&h, ExecMode::Paper).is_empty(),
        "no buys while the breaker is tripped"
    );
}

#[tokio::test]
async fn live_auto_detects_a_deposit_and_invests_it() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_fill_on_place(Some(None));
    h.engine.run_monthly(TODAY).await.expect("monthly");
    let monthly_places = h.fake().places().len();

    // 600,000 ARS arrive: the broker's cash grows and the flow shows up.
    let acct = h.fake().account_snapshot();
    h.fake().set_cash(
        acct.cash_ars + dec!(600000),
        acct.cash_total_ars + dec!(600000),
    );
    h.fake().add_flow(ExternalFlow {
        external_id: "dep-1".into(),
        kind: FlowKind::Deposit,
        amount_ars: dec!(600000),
        at: clock_time(),
    });
    h.engine.check_deposits().await.expect("check");
    let dep = deposit_orders(&h, ExecMode::Live);
    assert!(!dep.is_empty());
    assert!(dep.iter().all(|o| o.side == Side::Buy));
    let placed = h.fake().places().len();
    assert!(placed > monthly_places);

    // The same flow is not invested twice.
    h.engine.check_deposits().await.expect("check again");
    assert_eq!(h.fake().places().len(), placed);
}

#[tokio::test]
async fn flow_query_failure_never_breaks_the_deposit_check_contract() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_fail_flows(true);
    // The error is reported to the caller (the scheduler only logs it); no panic, no orders.
    assert!(h.engine.check_deposits().await.is_err());
    assert!(h.fake().places().is_empty());
    // and the risk check is independent of it
    h.engine.run_risk_check().await.expect("risk check");
}

#[tokio::test]
async fn live_confirm_deposit_creates_a_deposit_proposal() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    h.fake().set_fill_on_place(Some(None));
    let MonthlyOutcome::ProposalSent(monthly) = h.engine.run_monthly(TODAY).await.expect("monthly")
    else {
        panic!("expected a proposal");
    };
    h.engine
        .handle_approval(monthly, true)
        .await
        .expect("approve");
    let acct = h.fake().account_snapshot();
    h.fake().set_cash(
        acct.cash_ars + dec!(1200000),
        acct.cash_total_ars + dec!(1200000),
    );
    h.fake().add_flow(ExternalFlow {
        external_id: "dep-9".into(),
        kind: FlowKind::Deposit,
        amount_ars: dec!(1200000),
        at: clock_time(),
    });
    let placed = h.fake().places().len();
    h.engine.check_deposits().await.expect("check");
    assert_eq!(h.fake().places().len(), placed, "nothing before approval");
    let approvals = h.notifier.approvals();
    assert_eq!(approvals.len(), 2);
    let proposal = h
        .store
        .lock()
        .expect("lock")
        .get_proposal(approvals[1])
        .expect("get")
        .expect("proposal");
    assert_eq!(proposal.kind, ProposalKind::Deposit);

    let text = h
        .engine
        .handle_approval(proposal.id, true)
        .await
        .expect("approve");
    assert!(text.contains("depósito"), "{text}");
    assert!(h.fake().places().len() > placed);
    assert!(!deposit_orders(&h, ExecMode::Live).is_empty());
}

// --- B2.6: recalculation at approval time ---

#[tokio::test]
async fn proposal_text_says_the_plan_is_recalculated() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    h.engine.run_monthly(TODAY).await.expect("monthly");
    let texts = h.notifier.approval_texts();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("se recalcula"), "{}", texts[0]);
}

#[tokio::test]
async fn approval_is_refused_when_the_buys_grew_more_than_ten_percent() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    let MonthlyOutcome::ProposalSent(id) = h.engine.run_monthly(TODAY).await.expect("monthly")
    else {
        panic!("expected a proposal");
    };
    // Cash doubles before the owner approves: the recalculated buys are far larger.
    h.fake().set_cash(dec!(2400000), dec!(2400000));
    let text = h.engine.handle_approval(id, true).await.expect("approve");
    assert!(text.contains("nueva propuesta"), "{text}");
    assert!(h.fake().places().is_empty());
    let p = h
        .store
        .lock()
        .expect("lock")
        .get_proposal(id)
        .expect("get")
        .expect("proposal");
    assert_eq!(p.status, ProposalStatus::Expired);
}

#[tokio::test]
async fn approval_proceeds_when_the_plan_barely_changed() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    let MonthlyOutcome::ProposalSent(id) = h.engine.run_monthly(TODAY).await.expect("monthly")
    else {
        panic!("expected a proposal");
    };
    h.fake().set_cash(dec!(1210000), dec!(1210000));
    h.engine.handle_approval(id, true).await.expect("approve");
    assert!(!h.fake().places().is_empty());
}

// --- B2.8: the service watches its Telegram task ---

#[tokio::test]
async fn service_stops_with_an_error_when_the_telegram_task_panics() {
    let h = Harness::paper(dec!(1000000));
    let engine: Arc<Engine> = Arc::new(h.engine);
    let task = tokio::spawn(async {
        panic!("boom");
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let result = run_service(
        engine,
        Some(task),
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        std::future::pending::<()>(),
    )
    .await;
    let err = result.expect_err("must fail so the OS restarts the service");
    assert!(err.to_string().contains("Telegram"), "{err}");
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("Telegram") && m.contains("se detiene")),
        "{:?}",
        h.notifier.messages()
    );
}

#[tokio::test]
async fn service_stops_cleanly_on_shutdown() {
    let h = Harness::paper(dec!(1000000));
    let engine: Arc<Engine> = Arc::new(h.engine);
    let result = run_service(
        engine,
        None,
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        std::future::ready(()),
    )
    .await;
    assert!(result.is_ok());
}

// --- B2.10: reports ---

#[tokio::test]
async fn weekly_report_labels_estimated_fees() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_fill_on_place(Some(None)); // no fee reported by the broker: estimated
    h.engine.run_monthly(TODAY).await.expect("monthly");
    let weekly = h.engine.weekly_report().await.expect("weekly");
    assert!(weekly.contains("comisiones (estimadas)"), "{weekly}");
}

#[tokio::test]
async fn weekly_report_shows_flows_and_the_equity_bridge() {
    let h = Harness::paper(dec!(1200000));
    h.engine.run_daily_close(TODAY).await.expect("close");
    h.clock.advance(ChronoDuration::days(1));
    h.engine.paper_deposit(dec!(600000)).await.expect("deposit");
    let weekly = h.engine.weekly_report().await.expect("weekly");
    assert!(
        weekly.contains("Depósitos de la semana: ARS 600000"),
        "{weekly}"
    );
    assert!(weekly.contains("Puente del patrimonio"), "{weekly}");
    assert!(weekly.contains("inicial 1000"), "{weekly}");
    assert!(weekly.contains("aportes netos +500"), "{weekly}");
    assert!(weekly.contains("resultado +0"), "{weekly}");
    assert!(weekly.contains("final 1500"), "{weekly}");
}
