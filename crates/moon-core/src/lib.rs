//! Core domain types and logic for moon-trade.

pub mod allocator;
pub mod asset;
pub mod bars;
pub mod error;
pub mod indicators;
pub mod money;
pub mod risk;
pub mod strategy;
pub mod time;
pub mod types;
pub mod weights;

#[cfg(any(test, feature = "test-util"))]
pub mod testutil;
