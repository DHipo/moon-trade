mod common;

use std::collections::BTreeMap;

use common::{Harness, clock_time};
use moon_app::config::TradingMode;
use moon_app::engine::{STATE_DRAWDOWN, STATE_RISK_INDEX};
use moon_core::risk::DrawdownState;
use moon_core::risk_index::RiskIndex;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Seeds the risk index with `qty` AAA last seen at `prev_price_usd`, peak 1.0.
fn seed(h: &Harness, qty: Decimal, prev_price_usd: Decimal) {
    let index = RiskIndex {
        level: 1.0,
        holdings: BTreeMap::from([("AAA".to_string(), qty)]),
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
    for _ in 0..3 {
        let out = h.engine.run_risk_check().await.expect("check");
        assert!(!out.tripped);
        assert!((out.level - 1.0).abs() < 1e-9, "level {}", out.level);
        assert!(out.drawdown < 1e-9);
    }
    assert!(h.fake().places().is_empty());
    assert!(anomaly_messages(&h) >= 1);
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
