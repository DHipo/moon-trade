mod common;

use std::collections::BTreeMap;

use clap::Parser;
use common::{Harness, TODAY, clock_time};
use moon_app::cli::{Cli, Command, RiskCmd};
use moon_app::commands::risk::decision_text;
use moon_app::config::TradingMode;
use moon_app::engine::{MonthlyOutcome, RatioDecision, STATE_DRAWDOWN, STATE_RISK_INDEX};
use moon_core::risk::DrawdownState;
use moon_core::risk_index::{RiskIndex, Suspect};
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
    assert!(notices[0].contains("/ratio AAA"), "{}", notices[0]);
    assert!(notices[0].contains("/caida AAA"), "{}", notices[0]);
    assert!(notices[0].contains("BYMA/IOL"), "{}", notices[0]);
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
async fn owner_drop_counts_the_drop_at_the_next_check_and_can_trip_with_confirmation() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    seed_state(&h, dec!(100), dec!(20000) / dec!(1200), true, 0);
    let reply = h
        .engine
        .decide_ratio_change("aaa", RatioDecision::Drop)
        .expect("drop");
    assert!(
        reply.contains("AAA") && reply.contains("No se corrió"),
        "{reply}"
    );
    assert!(h.fake().places().is_empty(), "the command runs no check");
    let first = h.engine.run_risk_check().await.expect("first");
    assert!((first.level - 0.5).abs() < 1e-9, "level {}", first.level);
    assert!(!first.tripped, "needs a second confirming check");
    assert!(
        h.notifier
            .messages()
            .iter()
            .any(|m| m.contains("caída real")),
        "{:?}",
        h.notifier.messages()
    );
    let second = h.engine.run_risk_check().await.expect("second");
    assert!(second.tripped);
    assert!(second.emergency.is_some());
    assert!(!h.fake().places().is_empty());
}

#[tokio::test]
async fn owner_ratio_rebases_at_the_next_check_without_any_loss() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    seed_state(&h, dec!(100), dec!(20000) / dec!(1200), true, 0);
    h.engine
        .decide_ratio_change("AAA", RatioDecision::Ratio)
        .expect("ratio");
    for _ in 0..3 {
        let out = h.engine.run_risk_check().await.expect("check");
        assert!(!out.tripped);
        assert!((out.level - 1.0).abs() < 1e-9, "level {}", out.level);
    }
    assert!(h.fake().places().is_empty());
}

#[tokio::test]
async fn quantity_arriving_after_a_price_move_does_not_trip_nor_wait_for_the_owner() {
    // S2: ratio 2:1; AAA fell 8 % since the suspicion, then IOL credits the 200 units.
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(200));
    h.quotes.set(
        "AAA",
        moon_core::asset::Market::Bcba,
        dec!(9200),
        moon_core::asset::Currency::Ars,
    );
    let mut index = RiskIndex {
        level: 1.0,
        holdings: BTreeMap::from([("AAA".to_string(), dec!(100))]),
        prices_usd: BTreeMap::from([("AAA".to_string(), dec!(10000) / dec!(1200))]),
        at: Some(clock_time()),
        suspicions: BTreeMap::from([("AAA".to_string(), 2)]),
        ..RiskIndex::default()
    };
    index.suspects.insert(
        "AAA".to_string(),
        Suspect {
            base_qty: dec!(100),
            base_price: dec!(20000) / dec!(1200),
            k: 2.0,
        },
    );
    let key = h.engine.state_key(STATE_RISK_INDEX);
    h.store
        .lock()
        .expect("lock")
        .set_state(&key, &index)
        .expect("index");
    let dd_key = h.engine.state_key(STATE_DRAWDOWN);
    h.store
        .lock()
        .expect("lock")
        .set_state(
            &dd_key,
            &DrawdownState {
                peak_nav: 1.0,
                ..DrawdownState::default()
            },
        )
        .expect("drawdown");
    for _ in 0..3 {
        let out = h.engine.run_risk_check().await.expect("check");
        assert!(!out.tripped);
        assert!((out.level - 1.0).abs() < 1e-9, "level {}", out.level);
    }
    let stored: RiskIndex = h
        .store
        .lock()
        .expect("lock")
        .get_state(&key)
        .expect("get")
        .expect("index");
    assert!(stored.suspicions.is_empty(), "{stored:?}");
    assert!(h.fake().places().is_empty());
}

#[tokio::test]
async fn reply_says_when_the_breaker_is_active_that_it_applies_at_the_monthly() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    seed_state(&h, dec!(100), dec!(20000) / dec!(1200), true, 0);
    let dd_key = h.engine.state_key(STATE_DRAWDOWN);
    h.store
        .lock()
        .expect("lock")
        .set_state(
            &dd_key,
            &DrawdownState {
                peak_nav: 1.0,
                tripped: true,
                tripped_on: Some(TODAY),
                breaches: 0,
            },
        )
        .expect("drawdown");
    let reply = h
        .engine
        .decide_ratio_change("AAA", RatioDecision::Drop)
        .expect("drop");
    assert!(
        reply.contains("freno") && reply.contains("mensual"),
        "{reply}"
    );
    assert!(!reply.contains("próximo control de riesgo"), "{reply}");
}

#[tokio::test]
async fn deciding_without_a_pending_suspicion_fails() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    seed(&h, dec!(100), dec!(10));
    let err = h
        .engine
        .decide_ratio_change("AAA", RatioDecision::Drop)
        .expect_err("nothing pending");
    assert!(
        err.to_string().contains("No hay un posible cambio"),
        "{err}"
    );
    assert!(
        h.engine
            .decide_ratio_change("  ", RatioDecision::Ratio)
            .is_err()
    );
}

#[tokio::test]
async fn cli_ratio_and_drop_record_the_decision_and_the_service_applies_it() {
    for (word, ok) in [("ratio", true), ("drop", true)] {
        let cli = Cli::try_parse_from(["moon", "risk", word, "AAA"]).expect("parse");
        assert!(
            matches!(
                (&cli.command, word),
                (Command::Risk { cmd: RiskCmd::Ratio { symbol } }, "ratio") if symbol == "AAA"
            ) || matches!(
                (&cli.command, word),
                (Command::Risk { cmd: RiskCmd::Drop { symbol } }, "drop") if symbol == "AAA"
            )
        );
        assert!(ok);
    }
    assert!(Cli::try_parse_from(["moon", "risk", "accept", "AAA"]).is_err());
    let h = Harness::live(TradingMode::LiveAuto, dec!(0));
    h.fake().set_position("AAA", dec!(100));
    seed_state(&h, dec!(100), dec!(20000) / dec!(1200), true, 0);
    let text = decision_text(
        &mut h.store.lock().expect("lock"),
        ExecMode::Live,
        "AAA",
        RatioDecision::Drop,
    )
    .expect("drop");
    assert!(text.contains("Anotado"), "{text}");
    assert!(
        decision_text(
            &mut h.store.lock().expect("lock"),
            ExecMode::Live,
            "ZZZ",
            RatioDecision::Drop
        )
        .is_err()
    );
    let out = h.engine.run_risk_check().await.expect("check");
    assert!((out.level - 0.5).abs() < 1e-9, "level {}", out.level);
}

#[tokio::test]
async fn monthly_plan_skips_assets_with_a_pending_ratio_suspicion() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_position("AAA", dec!(100));
    seed_state(&h, dec!(100), dec!(20000) / dec!(1200), true, 0);
    let out = h.engine.run_monthly(TODAY).await.expect("monthly");
    assert!(matches!(
        out,
        MonthlyOutcome::Executed(_) | MonthlyOutcome::NoChanges
    ));
    assert!(
        h.fake().places().iter().all(|t| t.symbol != "AAA"),
        "{:?}",
        h.fake().places()
    );
    let messages = h.notifier.messages();
    let skips = messages
        .iter()
        .filter(|m| m.contains("AAA se omite del plan"))
        .count();
    assert_eq!(skips, 1, "{messages:?}");
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
    assert!(
        status.contains("/ratio AAA") && status.contains("/caida AAA"),
        "{status}"
    );
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
