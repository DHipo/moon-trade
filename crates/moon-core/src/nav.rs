//! Unitized NAV: deposits and withdrawals change units, not the NAV index.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::money::dec_to_f64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NavTracker {
    pub units: Decimal,
}

impl NavTracker {
    /// NAV starts at 1.0: one unit per currency unit of initial equity.
    pub fn start(initial_equity: Decimal) -> Self {
        Self {
            units: initial_equity.max(Decimal::ZERO),
        }
    }

    /// `equity / units`; `None` when there are no units.
    pub fn nav(&self, equity: Decimal) -> Option<f64> {
        if self.units <= Decimal::ZERO {
            return None;
        }
        Some(dec_to_f64(equity / self.units))
    }

    /// Records a deposit (`flow > 0`) or withdrawal (`flow < 0`) made at `equity_before_flow`.
    pub fn apply_flow(&mut self, flow: Decimal, equity_before_flow: Decimal) {
        if self.units <= Decimal::ZERO || equity_before_flow <= Decimal::ZERO {
            self.units = (equity_before_flow + flow).max(Decimal::ZERO);
        } else {
            self.units = (self.units + flow * self.units / equity_before_flow).max(Decimal::ZERO);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn nav_unchanged_by_flows() {
        let mut t = NavTracker::start(dec!(1000));
        assert_eq!(t.nav(dec!(1000)), Some(1.0));
        let nav = t.nav(dec!(1100)).unwrap();
        assert!((nav - 1.1).abs() < 1e-12);
        t.apply_flow(dec!(550), dec!(1100));
        let nav = t.nav(dec!(1650)).unwrap();
        assert!((nav - 1.1).abs() < 1e-12);
        t.apply_flow(dec!(-1650), dec!(1650));
        assert_eq!(t.units, dec!(0));
        assert_eq!(t.nav(dec!(0)), None);
    }

    #[test]
    fn flow_into_empty_tracker_restarts() {
        let mut t = NavTracker::default();
        t.apply_flow(dec!(500), dec!(0));
        assert_eq!(t.units, dec!(500));
        assert_eq!(NavTracker::start(dec!(-5)).units, dec!(0));
    }
}
