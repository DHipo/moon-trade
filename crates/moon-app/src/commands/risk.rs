//! `moon risk ratio` / `moon risk drop`: the owner's decision about a pending suspected ratio
//! change (`ratio`: it was a ratio change; `drop`: it was a real move).
//!
//! Purely local: it records the decision in the database; the service applies it at its next
//! scheduled risk check. It never calls the broker and never runs a risk check.

use moon_core::types::ExecMode;
use moon_store::Store;

use crate::commands::orders::exec_mode;
use crate::config::AppConfig;
use crate::engine::{RatioDecision, request_ratio_decision};

/// Records the decision and returns the Spanish confirmation text.
pub fn decision_text(
    store: &mut Store,
    mode: ExecMode,
    symbol: &str,
    decision: RatioDecision,
) -> anyhow::Result<String> {
    request_ratio_decision(store, mode, symbol, decision)
}

/// CLI entry of `moon risk ratio <symbol>` and `moon risk drop <symbol>`.
pub fn run_decision(cfg: &AppConfig, symbol: &str, decision: RatioDecision) -> anyhow::Result<()> {
    let mut store = Store::open(&cfg.database_path)?;
    println!(
        "{}",
        decision_text(&mut store, exec_mode(cfg), symbol, decision)?
    );
    Ok(())
}
