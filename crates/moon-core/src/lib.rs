//! Core domain types and logic for moon-trade.

pub mod allocator;
pub mod asset;
pub mod backtest;
pub mod bars;
pub mod error;
pub mod fees;
pub mod indicators;
pub mod metrics;
pub mod money;
pub mod nav;
pub mod rebalance;
pub mod risk;
pub mod strategy;
pub mod time;
pub mod types;
pub mod weights;

#[cfg(any(test, feature = "test-util"))]
pub mod testutil;
