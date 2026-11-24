mod common;

use std::collections::BTreeMap;

use clap::Parser;
use common::{Harness, clock_time};
use moon_app::cli::{Cli, Command, RiskCmd};
use moon_app::commands::risk::accept_text;
use moon_app::config::TradingMode;
use moon_app::engine::{STATE_DRAWDOWN, STATE_RISK_INDEX};
use moon_core::risk::DrawdownState;
use moon_core::risk_index::RiskIndex;
use moon_core::types::ExecMode;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Seeds the risk index with `qty` AAA last seen at `prev_price_usd`, peak 1.0.
fn seed(h: &Harness, qty: Decimal, prev_price_usd: Decimal) {
    seed_state(h, qty, prev_price_usd, false, 0);
}

/// Like `seed`, optionally with a pending suspected ratio change on AAA and `breaches`
/// unconfirmed breaches already recorded.
fn seed_state(h: &Harness, qty: Decimal, base_usd: Decimal, pending: bool, breaches: u32) {
    let index = RiskIndex {
        level: 1.0,
        holdings: BTreeMap::from([("AAA".to_string(), qty)]),
        prices_usd: BTreeMap::from([("AAA".to_string(), base_usd)]),
        at: Some(clock_time()),
        suspicions: if pending {
            BTreeMap::from([("AAA".to_string(), 1)])
        } else {
            BTreeMap::new()
        },
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
            breaches,
            ..DrawdownState::default()
        },
    )
    .expect("drawdown");
}

fn anomaly_messages(h: &Harness) -> usize {
    h.notifier
        .messages()
        .iter()
        .filter(|m| m.contains("Índice de riesgo"))
        .count()
}

#[tokio::test]
async fn ratio_change_with_quantity_catch_up_never_trips() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    // 50 units at 2x the price before; now 100 units at 10,000 ARS (8.33 USD at CCL 1200).
    h.fake().set_position("AAA", dec!(100));
    seed(&h, dec!(50), dec!(20000) / dec!(1200));
    for _ in 0..3 {
        let out = h.engine.run_risk_check().await.expect("check");
        assert!(!out.tripped);
        assert!((out.level - 1.0).abs() < 1e-9, "level {}", out.level);
    }
    assert!(h.fake().places().is_empty());
    assert_eq!(anomaly_messages(&h), 1, "announced once per day");
}

#[tokio::test]
async fn pending_ratio_change_does_not_trip_during_the_confirmation_window() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    // Price halved but the quantity still reads 100.
    h.fake().set_position("AAA", dec!(100));
    seed(&h, dec!(100), dec!(20000) / dec!(1200));
    // Far more observations than the old count-based acceptance needed: it never turns into a loss.
    for _ in 0..12 {
        let out = h.engine.run_risk_check().await.expect("check");
        assert!(!out.tripped);
        assert!((out.level - 1.0).abs() < 1e-9, "level {}", out.level);
        assert!(out.drawdown < 1e-9);
    }
    assert!(h.fake().places().is_empty());
    let messages = h.notifier.messages();
    let notices: Vec<_> = messages
        .iter()
        .filter(|m| m.contains("posible cambio de ratio en AAA"))
        .collect();
    assert_eq!(notices.len(), 1, "once per day: {messages:?}");
    assert!(notices[0].contains("/aceptar AAA"), "{}", notices[0]);
    assert!(
        notices[0].contains("sin cambio de cantidad"),
        "{}",
        notices[0]
    );
}

#[tokio::test]
async fn pending_resumes_when_the_price_returns_within_three_percent() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    // Kept base 8.5 USD, now 8.33: back within 3 %, so it counts as a normal (small) move.
    seed_state(&h, dec!(100), dec!(8.5), true, 0);
    let out = h.engine.run_risk_check().await.expect("check");
    assert!(!out.tripped);
    assert!(out.level < 1.0 && out.level > 0.97, "level {}", out.level);
    let index: RiskIndex = h
        .store
        .lock()
        .expect("lock")
        .get_state(&h.engine.state_key(STATE_RISK_INDEX))
        .expect("get")
        .expect("index");
    assert!(index.suspicions.is_empty());
    assert!(index.anomalies.is_empty());
}

#[tokio::test]
async fn owner_acceptance_counts_the_drop_and_can_trip_with_confirmation() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    seed_state(&h, dec!(100), dec!(20000) / dec!(1200), true, 0);
    assert_eq!(h.engine.accept_ratio_change("aaa").expect("accept"), "AAA");
    let first = h.engine.run_risk_check().await.expect("first");
    assert!((first.level - 0.5).abs() < 1e-9, "level {}", first.level);
    assert!(!first.tripped, "needs a second confirming check");
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("se cuenta como movimiento real")),
        "{:?}",
        h.notifier.messages()
    );
    let second = h.engine.run_risk_check().await.expect("second");
    assert!(second.tripped);
    assert!(second.emergency.is_some());
    assert!(!h.fake().places().is_empty());
}

#[tokio::test]
async fn accepting_without_a_pending_suspicion_fails() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    seed(&h, dec!(100), dec!(10));
    let err = h
        .engine
        .accept_ratio_change("AAA")
        .expect_err("nothing pending");
    assert!(
        err.to_string().contains("No hay un posible cambio"),
        "{err}"
    );
    assert!(h.engine.accept_ratio_change("  ").is_err());
}

#[tokio::test]
async fn cli_accept_records_the_request_and_the_service_applies_it() {
    let cli = Cli::try_parse_from(["moon", "risk", "accept", "AAA"]).expect("parse");
    assert!(matches!(
        cli.command,
        Command::Risk {
            cmd: RiskCmd::Accept { ref symbol }
        } if symbol == "AAA"
    ));
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    seed_state(&h, dec!(100), dec!(20000) / dec!(1200), true, 0);
    let text =
        accept_text(&mut h.store.lock().expect("lock"), ExecMode::Live, "AAA").expect("accept");
    assert!(text.contains("Aceptado"), "{text}");
    assert!(accept_text(&mut h.store.lock().expect("lock"), ExecMode::Live, "ZZZ").is_err());
    let out = h.engine.run_risk_check().await.expect("check");
    assert!((out.level - 0.5).abs() < 1e-9, "level {}", out.level);
}

#[tokio::test]
async fn status_shows_an_unconfirmed_breach_and_pending_ratio_suspicions() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    seed_state(&h, dec!(100), dec!(20000) / dec!(1200), true, 1);
    let status = h.engine.status_text().await.expect("status");
    assert!(
        status.contains("Ruptura sin confirmar: 1 de 2 controles"),
        "{status}"
    );
    assert!(
        status.contains("Posible cambio de ratio en AAA"),
        "{status}"
    );
    assert!(status.contains("/aceptar AAA"), "{status}");
    // Without breaches or suspicions neither line appears.
    seed(&h, dec!(100), dec!(10));
    let clean = h.engine.status_text().await.expect("status");
    assert!(!clean.contains("Ruptura sin confirmar"), "{clean}");
    assert!(!clean.contains("Posible cambio de ratio"), "{clean}");
}

#[tokio::test]
async fn real_breach_trips_on_the_second_consecutive_check() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    // 10 USD -> 8.33 USD: a 16.7 % fall, no split-like ratio.
    h.fake().set_position("AAA", dec!(100));
    seed(&h, dec!(100), dec!(10));
    let first = h.engine.run_risk_check().await.expect("first");
    assert!(!first.tripped, "one breach is not enough");
    assert!(h.fake().places().is_empty());
    let second = h.engine.run_risk_check().await.expect("second");
    assert!(second.tripped);
    assert!(second.emergency.is_some());
    assert_eq!(h.fake().places().len(), 1);
}
