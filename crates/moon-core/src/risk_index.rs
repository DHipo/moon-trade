//! Risk index: chained market return of what the bot holds, in USD, cash excluded.
//!
//! The drawdown breaker watches this index instead of account equity, so deposits, withdrawals,
//! pending buys, settlement and peso devaluation never look like a market loss.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::money::dec_to_f64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskIndex {
    /// Chained index, starts at 1.0.
    pub level: f64,
    /// Symbol -> quantity at the last observation.
    pub holdings: BTreeMap<String, Decimal>,
    /// Symbol -> USD unit price at the last observation.
    pub prices_usd: BTreeMap<String, Decimal>,
    pub at: Option<DateTime<Utc>>,
}

impl RiskIndex {
    pub fn new() -> Self {
        Self {
            level: 1.0,
            holdings: BTreeMap::new(),
            prices_usd: BTreeMap::new(),
            at: None,
        }
    }

    /// Chains the index with the market return of the PREVIOUS holdings between the previous
    /// and the current prices, then stores the current holdings/prices as the new base.
    /// Returns the period return.
    ///
    /// `r = sum(q_prev * (p_now - p_prev)) / sum(q_prev * p_prev)` over the previous holdings;
    /// `r = 0` when there is nothing invested or no previous observation. A previously held
    /// symbol without a current price is an error and leaves the index untouched.
    pub fn observe(
        &mut self,
        holdings: &BTreeMap<String, Decimal>,
        prices_usd: &BTreeMap<String, Decimal>,
        at: DateTime<Utc>,
    ) -> Result<f64, CoreError> {
        let mut base = 0.0_f64;
        let mut gain = 0.0_f64;
        for (sym, qty) in &self.holdings {
            if *qty <= Decimal::ZERO {
                continue;
            }
            let Some(prev) = self.prices_usd.get(sym) else {
                continue;
            };
            let now = prices_usd
                .get(sym)
                .ok_or_else(|| CoreError::MissingPrice(crate::asset::AssetId::new(sym.as_str())))?;
            let q = dec_to_f64(*qty);
            let (p0, p1) = (dec_to_f64(*prev), dec_to_f64(*now));
            base += q * p0;
            gain += q * (p1 - p0);
        }
        let r = if base > 0.0 && base.is_finite() && gain.is_finite() {
            gain / base
        } else {
            0.0
        };
        self.level *= 1.0 + r;
        self.holdings = holdings
            .iter()
            .filter(|(_, q)| **q > Decimal::ZERO)
            .map(|(s, q)| (s.clone(), *q))
            .collect();
        self.prices_usd = prices_usd.clone();
        self.at = Some(at);
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn t(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, day, 15, 0, 0).unwrap()
    }

    fn map(items: &[(&str, Decimal)]) -> BTreeMap<String, Decimal> {
        items.iter().map(|(s, v)| (s.to_string(), *v)).collect()
    }

    #[test]
    fn first_observation_keeps_level_one() {
        let mut idx = RiskIndex::new();
        let r = idx
            .observe(&map(&[("A", dec!(10))]), &map(&[("A", dec!(5))]), t(1))
            .unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.level, 1.0);
        assert_eq!(idx.at, Some(t(1)));
    }

    #[test]
    fn return_uses_previous_holdings() {
        let mut idx = RiskIndex::new();
        idx.observe(&map(&[("A", dec!(10))]), &map(&[("A", dec!(5))]), t(1))
            .unwrap();
        // Holdings change at the same time as prices: only the previous quantity counts.
        let r = idx
            .observe(&map(&[("A", dec!(100))]), &map(&[("A", dec!(4.5))]), t(2))
            .unwrap();
        assert!((r + 0.1).abs() < 1e-12);
        assert!((idx.level - 0.9).abs() < 1e-12);
        assert_eq!(idx.holdings["A"], dec!(100));
    }

    #[test]
    fn weighted_across_symbols() {
        let mut idx = RiskIndex::new();
        idx.observe(
            &map(&[("A", dec!(10)), ("B", dec!(10))]),
            &map(&[("A", dec!(10)), ("B", dec!(30))]),
            t(1),
        )
        .unwrap();
        // A +10 % (value 100), B -10 % (value 300): (10 - 30) / 400 = -5 %.
        let r = idx
            .observe(
                &map(&[("A", dec!(10)), ("B", dec!(10))]),
                &map(&[("A", dec!(11)), ("B", dec!(27))]),
                t(2),
            )
            .unwrap();
        assert!((r + 0.05).abs() < 1e-12);
    }

    #[test]
    fn cash_movements_do_not_move_the_index() {
        // Holdings and prices unchanged while cash (not an input) swings: level stays.
        let mut idx = RiskIndex::new();
        let h = map(&[("A", dec!(10))]);
        let p = map(&[("A", dec!(5))]);
        for day in 1..5 {
            idx.observe(&h, &p, t(day)).unwrap();
        }
        assert_eq!(idx.level, 1.0);
    }

    #[test]
    fn no_positions_means_zero_return() {
        let mut idx = RiskIndex::new();
        idx.observe(&BTreeMap::new(), &map(&[("A", dec!(5))]), t(1))
            .unwrap();
        let r = idx
            .observe(&BTreeMap::new(), &map(&[("A", dec!(1))]), t(2))
            .unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.level, 1.0);
    }

    #[test]
    fn missing_current_price_errors_and_keeps_state() {
        let mut idx = RiskIndex::new();
        idx.observe(&map(&[("A", dec!(10))]), &map(&[("A", dec!(5))]), t(1))
            .unwrap();
        let before = idx.clone();
        let err = idx
            .observe(&map(&[("A", dec!(10))]), &BTreeMap::new(), t(2))
            .unwrap_err();
        assert!(matches!(err, CoreError::MissingPrice(_)));
        assert_eq!(idx, before);
    }

    #[test]
    fn serde_roundtrip() {
        let mut idx = RiskIndex::new();
        idx.observe(&map(&[("A", dec!(10))]), &map(&[("A", dec!(5))]), t(1))
            .unwrap();
        let s = serde_json::to_string(&idx).unwrap();
        assert_eq!(serde_json::from_str::<RiskIndex>(&s).unwrap(), idx);
    }
}
