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

/// Quantity ratio from which a quantity jump is a candidate corporate action.
const CORP_ACTION_MIN_RATIO: f64 = 1.5;
/// Tolerance on `value_now / value_prev` for a quantity jump to count as a corporate action.
const CORP_ACTION_VALUE_TOL: f64 = 0.05;
/// Tolerance around an integer ratio for a price jump with unchanged quantity.
const PENDING_RATIO_TOL: f64 = 0.03;
/// Consecutive suspicious observations after which a price jump is accepted as a real move.
const PENDING_CONFIRMATIONS: u32 = 4;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AnomalyKind {
    /// Quantity and price moved inversely (ratio change or split): return forced to zero.
    CorporateAction,
    /// Price jumped by an integer ratio while the quantity did not: waiting for confirmation.
    SuspectedRatioChange,
    /// A suspected jump persisted long enough and was accepted as a real market move.
    AcceptedAfterConfirmation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexAnomaly {
    pub symbol: String,
    pub kind: AnomalyKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskIndex {
    /// Chained index, starts at 1.0.
    pub level: f64,
    /// Symbol -> quantity at the last observation.
    pub holdings: BTreeMap<String, Decimal>,
    /// Symbol -> USD unit price used as the base of the next observation.
    pub prices_usd: BTreeMap<String, Decimal>,
    pub at: Option<DateTime<Utc>>,
    /// Symbol -> consecutive suspected-ratio-change observations.
    #[serde(default)]
    pub suspicions: BTreeMap<String, u32>,
    /// Anomalies found by the last `observe` (cleared at the start of each one).
    #[serde(default)]
    pub anomalies: Vec<IndexAnomaly>,
}

/// Nearest integer `k >= 2` within `tol` (relative) of `x`, if any.
fn near_integer(x: f64, tol: f64) -> Option<u32> {
    if !x.is_finite() || !(1.5..1e9).contains(&x) {
        return None;
    }
    let k = x.round();
    if k >= 2.0 && (x - k).abs() / k <= tol {
        Some(k as u32)
    } else {
        None
    }
}

impl RiskIndex {
    pub fn new() -> Self {
        Self {
            level: 1.0,
            ..Self::default()
        }
    }

    /// Chains the index with the market return of the PREVIOUS holdings between the previous
    /// and the current prices, then stores the current holdings/prices as the new base.
    /// Returns the period return.
    ///
    /// `r = sum(q_prev * (p_now - p_prev)) / sum(q_prev * p_prev)` over the previous holdings;
    /// `r = 0` when there is nothing invested or no previous observation. A previously held
    /// symbol without a current price is an error and leaves the index untouched.
    ///
    /// Corporate actions (quantity x k, price / k) and suspected ratio changes (price jumps by an
    /// integer ratio while the quantity is unchanged) contribute a zero return and are recorded
    /// in `anomalies`; see [`AnomalyKind`].
    pub fn observe(
        &mut self,
        holdings: &BTreeMap<String, Decimal>,
        prices_usd: &BTreeMap<String, Decimal>,
        at: DateTime<Utc>,
    ) -> Result<f64, CoreError> {
        let mut base = 0.0_f64;
        let mut gain = 0.0_f64;
        let mut anomalies: Vec<IndexAnomaly> = Vec::new();
        let mut suspicions: BTreeMap<String, u32> = BTreeMap::new();
        let mut kept_base: BTreeMap<String, Decimal> = BTreeMap::new();
        for (sym, qty) in &self.holdings {
            if *qty <= Decimal::ZERO {
                continue;
            }
            let Some(prev) = self.prices_usd.get(sym) else {
                continue;
            };
            let q = dec_to_f64(*qty);
            let p0 = dec_to_f64(*prev);
            let q_now = holdings.get(sym).copied().unwrap_or(Decimal::ZERO);
            let Some(now) = prices_usd.get(sym) else {
                if q_now > Decimal::ZERO {
                    return Err(CoreError::MissingPrice(crate::asset::AssetId::new(
                        sym.as_str(),
                    )));
                }
                // Sold since the last observation and no price now: its proceeds are cash, so
                // its period return counts as zero.
                tracing::warn!(
                    "{sym} ya no está en cartera y no tiene precio: su retorno del período se toma como 0"
                );
                base += q * p0;
                continue;
            };
            let p1 = dec_to_f64(*now);
            base += q * p0;

            if q_now > Decimal::ZERO && p0 > 0.0 && p1 > 0.0 {
                let kq = dec_to_f64(q_now) / q;
                let kp = p0 / p1;
                let value_ratio = kq / kp;
                let jump = kq >= CORP_ACTION_MIN_RATIO || kq <= 1.0 / CORP_ACTION_MIN_RATIO;
                if jump && (value_ratio - 1.0).abs() <= CORP_ACTION_VALUE_TOL {
                    anomalies.push(IndexAnomaly {
                        symbol: sym.clone(),
                        kind: AnomalyKind::CorporateAction,
                        detail: format!(
                            "cantidad x{kq:.4} y precio /{kp:.4}: se toma como cambio de ratio o split"
                        ),
                    });
                    continue;
                }
                if q_now == *qty {
                    let ratio = near_integer(kp, PENDING_RATIO_TOL)
                        .or_else(|| near_integer(1.0 / kp, PENDING_RATIO_TOL));
                    if let Some(k) = ratio {
                        let count = self.suspicions.get(sym).copied().unwrap_or(0) + 1;
                        if count >= PENDING_CONFIRMATIONS {
                            gain += q * (p1 - p0);
                            anomalies.push(IndexAnomaly {
                                symbol: sym.clone(),
                                kind: AnomalyKind::AcceptedAfterConfirmation,
                                detail: format!(
                                    "el salto de precio (ratio {k}) persistió {count} observaciones: se toma como movimiento real"
                                ),
                            });
                        } else {
                            suspicions.insert(sym.clone(), count);
                            kept_base.insert(sym.clone(), *prev);
                            anomalies.push(IndexAnomaly {
                                symbol: sym.clone(),
                                kind: AnomalyKind::SuspectedRatioChange,
                                detail: format!(
                                    "el precio saltó por un factor cercano a {k} sin cambio de cantidad ({count}/{PENDING_CONFIRMATIONS}): se espera confirmación"
                                ),
                            });
                        }
                        continue;
                    }
                }
            }
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
        for (sym, p) in kept_base {
            self.prices_usd.insert(sym, p);
        }
        self.suspicions = suspicions;
        self.anomalies = anomalies;
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
    fn sold_symbol_without_a_price_counts_as_zero_return() {
        let mut idx = RiskIndex::new();
        idx.observe(
            &map(&[("A", dec!(10)), ("B", dec!(10))]),
            &map(&[("A", dec!(10)), ("B", dec!(10))]),
            t(1),
        )
        .unwrap();
        // A was sold (no longer held, no price); B gained 10 %: weighted return 5 %.
        let r = idx
            .observe(&map(&[("B", dec!(10))]), &map(&[("B", dec!(11))]), t(2))
            .unwrap();
        assert!((r - 0.05).abs() < 1e-12, "{r}");
        assert!((idx.level - 1.05).abs() < 1e-12);
        // B is still held without a price: that keeps failing.
        let err = idx.observe(&map(&[("B", dec!(10))]), &BTreeMap::new(), t(3));
        assert!(err.is_err());
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

    fn prev(idx: &mut RiskIndex, q: Decimal, p: Decimal) {
        idx.observe(&map(&[("A", q)]), &map(&[("A", p)]), t(1))
            .unwrap();
    }

    #[test]
    fn ratio_change_is_a_corporate_action_with_zero_return() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let r = idx
            .observe(&map(&[("A", dec!(20))]), &map(&[("A", dec!(5))]), t(2))
            .unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.level, 1.0);
        assert_eq!(idx.anomalies.len(), 1);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::CorporateAction);
        // Rebased: next move is measured from the new base.
        let r = idx
            .observe(&map(&[("A", dec!(20))]), &map(&[("A", dec!(5.5))]), t(3))
            .unwrap();
        assert!((r - 0.1).abs() < 1e-12);
        assert!(idx.anomalies.is_empty());
    }

    #[test]
    fn reverse_split_is_a_corporate_action() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let r = idx
            .observe(&map(&[("A", dec!(2))]), &map(&[("A", dec!(50))]), t(2))
            .unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::CorporateAction);
    }

    #[test]
    fn quantity_jump_that_loses_value_is_not_a_corporate_action() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        // Quantity x2 but price -75 %: value halves, a real loss.
        let r = idx
            .observe(&map(&[("A", dec!(20))]), &map(&[("A", dec!(2.5))]), t(2))
            .unwrap();
        assert!((r + 0.75).abs() < 1e-12);
        assert!(idx.anomalies.is_empty());
    }

    #[test]
    fn pending_ratio_change_keeps_base_and_zero_return() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let h = map(&[("A", dec!(10))]);
        let r = idx.observe(&h, &map(&[("A", dec!(5))]), t(2)).unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.level, 1.0);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::SuspectedRatioChange);
        assert_eq!(idx.prices_usd["A"], dec!(10));
        assert_eq!(idx.suspicions["A"], 1);
        // The quantity catches up: the corporate action is recognised against the kept base.
        let r = idx
            .observe(&map(&[("A", dec!(20))]), &map(&[("A", dec!(5))]), t(3))
            .unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::CorporateAction);
        assert!(idx.suspicions.is_empty());
        assert_eq!(idx.prices_usd["A"], dec!(5));
    }

    #[test]
    fn inverse_pending_jump_is_suspected_too() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let r = idx
            .observe(&map(&[("A", dec!(10))]), &map(&[("A", dec!(20))]), t(2))
            .unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::SuspectedRatioChange);
    }

    #[test]
    fn real_halving_is_accepted_on_the_fourth_observation() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let h = map(&[("A", dec!(10))]);
        let p = map(&[("A", dec!(5))]);
        for day in 2..=4 {
            let r = idx.observe(&h, &p, t(day)).unwrap();
            assert_eq!(r, 0.0, "day {day}");
            assert_eq!(idx.level, 1.0);
            assert_eq!(idx.anomalies[0].kind, AnomalyKind::SuspectedRatioChange);
            assert_eq!(idx.suspicions["A"], day - 1);
        }
        let r = idx.observe(&h, &p, t(5)).unwrap();
        assert!((r + 0.5).abs() < 1e-12);
        assert!((idx.level - 0.5).abs() < 1e-12);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::AcceptedAfterConfirmation);
        assert_eq!(idx.prices_usd["A"], dec!(5));
        assert!(idx.suspicions.is_empty());
        // Counted once: afterwards nothing is suspicious.
        idx.observe(&h, &p, t(6)).unwrap();
        assert!(idx.anomalies.is_empty());
        assert!((idx.level - 0.5).abs() < 1e-12);
    }

    #[test]
    fn suspicion_resets_when_the_price_recovers() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let h = map(&[("A", dec!(10))]);
        idx.observe(&h, &map(&[("A", dec!(5))]), t(2)).unwrap();
        let r = idx.observe(&h, &map(&[("A", dec!(10))]), t(3)).unwrap();
        assert_eq!(r, 0.0);
        assert!(idx.suspicions.is_empty());
        assert!(idx.anomalies.is_empty());
    }

    #[test]
    fn small_moves_are_not_suspicious_and_old_state_loads() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let r = idx
            .observe(&map(&[("A", dec!(10))]), &map(&[("A", dec!(8))]), t(2))
            .unwrap();
        assert!((r + 0.2).abs() < 1e-12);
        assert!(idx.anomalies.is_empty());
        let old = r#"{"level":1.0,"holdings":{},"prices_usd":{},"at":null}"#;
        let loaded: RiskIndex = serde_json::from_str(old).unwrap();
        assert!(loaded.suspicions.is_empty() && loaded.anomalies.is_empty());
    }
}
