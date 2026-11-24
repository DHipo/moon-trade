//! P-10: a monthly that was executed or proposed consumes the deposits detected before it.

mod common;

use common::{Harness, TODAY};
use moon_app::config::TradingMode;
use moon_app::engine::{MonthlyOutcome, STATE_DEPOSIT_PENDING};
use rust_decimal_macros::dec;

fn pending(h: &Harness) -> Option<Vec<String>> {
    let key = h.engine.state_key(STATE_DEPOSIT_PENDING);
    h.store.lock().expect("lock").get_state(&key).expect("get")
}

fn seed_pending(h: &Harness) {
    let key = h.engine.state_key(STATE_DEPOSIT_PENDING);
    h.store
        .lock()
        .expect("lock")
        .set_state(&key, &vec!["dep-1".to_string()])
        .expect("set");
}

#[tokio::test]
async fn proposing_the_monthly_clears_pending_deposits() {
    let h = Harness::live(TradingMode::LiveConfirm, dec!(1200000));
    seed_pending(&h);
    let out = h.engine.run_monthly(TODAY).await.expect("monthly");
    assert!(matches!(out, MonthlyOutcome::ProposalSent(_)), "{out:?}");
    assert_eq!(pending(&h), None, "no second proposal for the same cash");
}

#[tokio::test]
async fn executing_the_monthly_clears_pending_deposits() {
    let h = Harness::live(TradingMode::LiveAuto, dec!(1200000));
    h.fake().set_fill_on_place(Some(None));
    seed_pending(&h);
    let out = h.engine.run_monthly(TODAY).await.expect("monthly");
    assert!(matches!(out, MonthlyOutcome::Executed(_)), "{out:?}");
    assert_eq!(pending(&h), None);
}
