//! Risk index: chained market return of what the bot holds, in USD, cash excluded.
//!
//! The drawdown breaker watches this index instead of account equity, so deposits, withdrawals,
//! pending buys, settlement and peso devaluation never look like a market loss.

use std::collections::{BTreeMap, BTreeSet};

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
/// Relative tolerance between the quantity multiplier seen and the one the suspicion expects.
const SUSPECT_QTY_TOL: f64 = 0.05;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AnomalyKind {
    /// Quantity and price moved inversely (ratio change or split): return forced to zero.
    CorporateAction,
    /// Price jumped by an integer ratio while the quantity did not: excluded (zero return, base
    /// kept) until the quantity follows, the price returns near the base or the owner decides.
    SuspectedRatioChange,
    /// The owner said a suspected jump was a real move: it was counted from the kept base.
    #[serde(alias = "AcceptedAfterConfirmation")]
    AcceptedByOwner,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexAnomaly {
    pub symbol: String,
    pub kind: AnomalyKind,
    pub detail: String,
}

/// A suspected ratio change: the base the move is measured from and the quantity multiplier
/// the ratio change would bring (`price / k` goes with `quantity x k`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suspect {
    pub base_qty: Decimal,
    pub base_price: Decimal,
    pub k: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RiskIndex {
    /// Chained index, starts at 1.0.
    pub level: f64,
    /// Symbol -> quantity at the last observation.
    pub holdings: BTreeMap<String, Decimal>,
    /// Symbol -> USD unit price at the last observation (the base of the next one).
    pub prices_usd: BTreeMap<String, Decimal>,
    pub at: Option<DateTime<Utc>>,
    /// Symbol -> observations spent as a pending suspected ratio change (informational: it never
    /// expires by count). The base of each one is in `suspects`.
    #[serde(default)]
    pub suspicions: BTreeMap<String, u32>,
    /// Symbol -> base and expected multiplier of its pending suspicion. A suspicion saved
    /// without it (older state) takes the previous holding and price as its base.
    #[serde(default)]
    pub suspects: BTreeMap<String, Suspect>,
    /// Symbols whose quantity jumped by an integer ratio with the price unchanged: zero return
    /// at that check, rebased at the next one (the price may follow in between).
    #[serde(default)]
    pub qty_first: BTreeSet<String>,
    /// Anomalies found by the last `observe` (cleared at the start of each one).
    #[serde(default)]
    pub anomalies: Vec<IndexAnomaly>,
    /// Pending suspicions the owner said were REAL moves: counted from the base by the next
    /// `observe`.
    #[serde(default, alias = "accepted")]
    pub drops: BTreeSet<String>,
    /// Pending suspicions the owner said were ratio changes: rebased with zero return by the
    /// next `observe`.
    #[serde(default)]
    pub ratios: BTreeSet<String>,
}

/// Spanish description of a price move `kp = p_prev / p_now` ("precio ÷2", "precio ×3").
fn describe_move(kp: f64) -> String {
    let (sign, f) = if kp >= 1.0 {
        ('÷', kp)
    } else {
        ('×', 1.0 / kp)
    };
    if (f - f.round()).abs() < 0.005 {
        format!("precio {sign}{}", f.round())
    } else {
        format!("precio {sign}{f:.2}")
    }
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

fn anomaly(symbol: &str, kind: AnomalyKind, detail: String) -> IndexAnomaly {
    IndexAnomaly {
        symbol: symbol.to_string(),
        kind,
        detail,
    }
}

impl RiskIndex {
    /// Records that the pending suspicion of `symbol` was a REAL move: the next `observe` counts
    /// it from the kept base and rebases. False when none is pending.
    pub fn request_drop(&mut self, symbol: &str) -> bool {
        let pending = self.suspicions.contains_key(symbol);
        if pending {
            self.drops.insert(symbol.to_string());
        }
        pending
    }

    /// Records that the pending suspicion of `symbol` was a ratio change: the next `observe`
    /// rebases to the current quantity and price with zero return. False when none is pending.
    pub fn request_ratio(&mut self, symbol: &str) -> bool {
        let pending = self.suspicions.contains_key(symbol);
        if pending {
            self.ratios.insert(symbol.to_string());
        }
        pending
    }

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
    /// Corporate actions (quantity x k, price / k) contribute a zero return and are recorded in
    /// `anomalies`. A price jump by an integer ratio with the quantity unchanged becomes a
    /// suspicion (zero return, base kept) that resolves as a ratio change when the quantity
    /// reaches the expected multiplier, whatever the price did meanwhile; resumes normally when
    /// the price returns near the base; ends with zero return when the symbol is no longer held;
    /// and otherwise waits for the owner (`request_ratio` / `request_drop`). See [`AnomalyKind`].
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
        let mut suspects: BTreeMap<String, Suspect> = BTreeMap::new();
        let mut qty_first: BTreeSet<String> = BTreeSet::new();
        for (sym, qty) in &self.holdings {
            if *qty <= Decimal::ZERO {
                continue;
            }
            let Some(prev) = self.prices_usd.get(sym) else {
                continue;
            };
            let suspect = self.suspects.get(sym).cloned().or_else(|| {
                self.suspicions.contains_key(sym).then_some(Suspect {
                    base_qty: *qty,
                    base_price: *prev,
                    k: 0.0,
                })
            });
            let (q, p0) = match &suspect {
                Some(s) => (dec_to_f64(s.base_qty), dec_to_f64(s.base_price)),
                None => (dec_to_f64(*qty), dec_to_f64(*prev)),
            };
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

            if q_now <= Decimal::ZERO {
                // No longer held. A suspected symbol that was sold carries no real return (its
                // base is stale): zero, and the suspicion ends.
                if suspect.is_none() {
                    gain += q * (p1 - p0);
                }
                continue;
            }
            if p0 <= 0.0 || p1 <= 0.0 {
                gain += q * (p1 - p0);
                continue;
            }
            let kq = dec_to_f64(q_now) / q;
            let kp = p0 / p1;
            let price_near_base = (p1 / p0 - 1.0).abs() <= PENDING_RATIO_TOL;

            if let Some(s) = suspect {
                // A suspicion saved without its multiplier takes it from the current price move
                // when that is a real jump (otherwise nothing can match).
                let k = if s.k > 0.0 {
                    s.k
                } else if (kp - 1.0).abs() > 0.5 {
                    kp
                } else {
                    0.0
                };
                let kq_is_k = k > 0.0 && (kq / k - 1.0).abs() <= SUSPECT_QTY_TOL;
                let qty_unchanged = (kq - 1.0).abs() <= SUSPECT_QTY_TOL;
                if self.ratios.contains(sym) {
                    anomalies.push(anomaly(
                        sym,
                        AnomalyKind::CorporateAction,
                        format!(
                            "confirmado por el dueño como cambio de ratio ({}): sin retorno, se toma la cantidad y el precio actuales como base",
                            describe_move(kp)
                        ),
                    ));
                } else if kq_is_k && price_near_base {
                    // The quantity moved first and the price is where it was: hold one check.
                    qty_first.insert(sym.clone());
                    anomalies.push(anomaly(
                        sym,
                        AnomalyKind::CorporateAction,
                        format!(
                            "cantidad x{kq:.2} con el precio sin cambio: se espera que el precio la siga"
                        ),
                    ));
                } else if kq_is_k {
                    anomalies.push(anomaly(
                        sym,
                        AnomalyKind::CorporateAction,
                        format!(
                            "cantidad x{kq:.4} y {}: se toma como cambio de ratio o split",
                            describe_move(kp)
                        ),
                    ));
                } else if self.drops.contains(sym) && !price_near_base {
                    gain += q * (p1 - p0);
                    anomalies.push(anomaly(
                        sym,
                        AnomalyKind::AcceptedByOwner,
                        format!(
                            "confirmado por el dueño como caída real ({}): se cuenta como movimiento real",
                            describe_move(kp)
                        ),
                    ));
                } else if price_near_base && qty_unchanged {
                    // The price came back to where it was: resume normally.
                    gain += q * (p1 - p0);
                } else {
                    let count = self.suspicions.get(sym).copied().unwrap_or(0) + 1;
                    suspicions.insert(sym.clone(), count);
                    suspects.insert(sym.clone(), s);
                    let detail = if qty_unchanged {
                        format!("{} sin cambio de cantidad", describe_move(kp))
                    } else {
                        format!("{}, cantidad x{kq:.2}", describe_move(kp))
                    };
                    anomalies.push(anomaly(sym, AnomalyKind::SuspectedRatioChange, detail));
                }
                continue;
            }

            if self.qty_first.contains(sym) {
                // The quantity jumped at the previous check: whatever the price did since, the
                // current quantity and price are the new base.
                anomalies.push(anomaly(
                    sym,
                    AnomalyKind::CorporateAction,
                    "cambio de cantidad ya reflejado: se toma la cantidad y el precio actuales como base"
                        .to_string(),
                ));
                continue;
            }
            let jump = kq >= CORP_ACTION_MIN_RATIO || kq <= 1.0 / CORP_ACTION_MIN_RATIO;
            if jump && (kq / kp - 1.0).abs() <= CORP_ACTION_VALUE_TOL {
                anomalies.push(anomaly(
                    sym,
                    AnomalyKind::CorporateAction,
                    format!(
                        "cantidad x{kq:.4} y precio /{kp:.4}: se toma como cambio de ratio o split"
                    ),
                ));
                continue;
            }
            let qty_integer_jump = near_integer(kq, PENDING_RATIO_TOL).is_some()
                || near_integer(1.0 / kq, PENDING_RATIO_TOL).is_some();
            if qty_integer_jump && price_near_base {
                qty_first.insert(sym.clone());
                anomalies.push(anomaly(
                    sym,
                    AnomalyKind::CorporateAction,
                    format!(
                        "cantidad x{kq:.2} con el precio sin cambio: se espera que el precio la siga"
                    ),
                ));
                continue;
            }
            // A new suspicion needs an integer price ratio with the quantity unchanged.
            if q_now == *qty {
                let k = if let Some(k) = near_integer(kp, PENDING_RATIO_TOL) {
                    Some(f64::from(k))
                } else {
                    near_integer(1.0 / kp, PENDING_RATIO_TOL).map(|m| 1.0 / f64::from(m))
                };
                if let Some(k) = k {
                    suspicions.insert(sym.clone(), 1);
                    suspects.insert(
                        sym.clone(),
                        Suspect {
                            base_qty: *qty,
                            base_price: *prev,
                            k,
                        },
                    );
                    anomalies.push(anomaly(
                        sym,
                        AnomalyKind::SuspectedRatioChange,
                        format!("{} sin cambio de cantidad", describe_move(kp)),
                    ));
                    continue;
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
        self.suspicions = suspicions;
        self.suspects = suspects;
        self.qty_first = qty_first;
        self.drops.clear();
        self.ratios.clear();
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
        assert_eq!(idx.suspects["A"].base_price, dec!(10));
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
    fn pending_ratio_change_is_never_accepted_by_count() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let h = map(&[("A", dec!(10))]);
        let p = map(&[("A", dec!(5))]);
        for day in 2..=40 {
            let r = idx.observe(&h, &p, t(day.min(28))).unwrap();
            assert_eq!(r, 0.0, "observation {day}");
            assert_eq!(idx.level, 1.0);
            assert_eq!(idx.anomalies[0].kind, AnomalyKind::SuspectedRatioChange);
            assert_eq!(idx.suspects["A"].base_price, dec!(10), "base kept");
        }
    }

    #[test]
    fn pending_stays_pending_when_the_price_moves_further() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let h = map(&[("A", dec!(10))]);
        idx.observe(&h, &map(&[("A", dec!(5))]), t(2)).unwrap();
        // 5 -> 4.5 leaves the integer-ratio band against the kept base: still not counted.
        let r = idx.observe(&h, &map(&[("A", dec!(4.5))]), t(3)).unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.level, 1.0);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::SuspectedRatioChange);
        assert_eq!(idx.suspects["A"].base_price, dec!(10));
    }

    #[test]
    fn pending_resumes_when_the_price_returns_within_three_percent() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let h = map(&[("A", dec!(10))]);
        idx.observe(&h, &map(&[("A", dec!(5))]), t(2)).unwrap();
        let r = idx.observe(&h, &map(&[("A", dec!(9.8))]), t(3)).unwrap();
        assert!((r + 0.02).abs() < 1e-12, "{r}");
        assert!(idx.suspicions.is_empty() && idx.anomalies.is_empty());
        assert_eq!(idx.prices_usd["A"], dec!(9.8));
    }

    #[test]
    fn owner_drop_counts_the_move_from_the_kept_base_once() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        let h = map(&[("A", dec!(10))]);
        let p = map(&[("A", dec!(5))]);
        idx.observe(&h, &p, t(2)).unwrap();
        assert!(idx.request_drop("A"));
        assert!(!idx.request_drop("B"), "nothing pending for B");
        let r = idx.observe(&h, &p, t(3)).unwrap();
        assert!((r + 0.5).abs() < 1e-12);
        assert!((idx.level - 0.5).abs() < 1e-12);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::AcceptedByOwner);
        assert!(idx.anomalies[0].detail.contains("movimiento real"));
        assert_eq!(idx.prices_usd["A"], dec!(5));
        assert!(idx.suspicions.is_empty() && idx.drops.is_empty());
        // Counted once: afterwards nothing is suspicious.
        idx.observe(&h, &p, t(4)).unwrap();
        assert!(idx.anomalies.is_empty());
        assert!((idx.level - 0.5).abs() < 1e-12);
    }

    #[test]
    fn quantity_catching_up_after_a_pending_jump_is_a_corporate_action() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(10), dec!(10));
        idx.observe(&map(&[("A", dec!(10))]), &map(&[("A", dec!(5))]), t(2))
            .unwrap();
        idx.request_drop("A");
        // The quantity update arrives together with the owner's acceptance: no loss counted.
        let r = idx
            .observe(&map(&[("A", dec!(20))]), &map(&[("A", dec!(5))]), t(3))
            .unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::CorporateAction);
    }

    #[test]
    fn old_state_with_the_previous_anomaly_name_loads() {
        let old = r#"{"level":1.0,"holdings":{},"prices_usd":{},"at":null,
            "anomalies":[{"symbol":"A","kind":"AcceptedAfterConfirmation","detail":"x"}]}"#;
        let loaded: RiskIndex = serde_json::from_str(old).unwrap();
        assert_eq!(loaded.anomalies[0].kind, AnomalyKind::AcceptedByOwner);
        assert!(loaded.drops.is_empty());
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

    #[test]
    fn quantity_arriving_after_a_price_move_resolves_the_suspicion() {
        // S2: ratio 2:1; the price halves first, the quantity arrives later with the price 8 % lower.
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(100), dec!(10));
        idx.observe(&map(&[("A", dec!(100))]), &map(&[("A", dec!(5))]), t(2))
            .unwrap();
        assert_eq!(idx.suspects["A"].k, 2.0);
        let r = idx
            .observe(&map(&[("A", dec!(200))]), &map(&[("A", dec!(4.6))]), t(3))
            .unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.level, 1.0);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::CorporateAction);
        assert!(idx.suspicions.is_empty() && idx.suspects.is_empty());
        assert_eq!(idx.holdings["A"], dec!(200));
        assert_eq!(idx.prices_usd["A"], dec!(4.6));
    }

    #[test]
    fn inverse_ratio_resolves_when_the_quantity_shrinks() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(100), dec!(10));
        idx.observe(&map(&[("A", dec!(100))]), &map(&[("A", dec!(30))]), t(2))
            .unwrap();
        assert!((idx.suspects["A"].k - 1.0 / 3.0).abs() < 1e-12);
        let r = idx
            .observe(&map(&[("A", dec!(33.4))]), &map(&[("A", dec!(30))]), t(3))
            .unwrap();
        assert_eq!(r, 0.0);
        assert!(idx.suspicions.is_empty());
    }

    #[test]
    fn quantity_first_holds_one_check_then_rebases() {
        // S11: the quantity doubles with the price unchanged; the price halves a check later.
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(100), dec!(10));
        let r = idx
            .observe(&map(&[("A", dec!(200))]), &map(&[("A", dec!(10))]), t(2))
            .unwrap();
        assert_eq!(r, 0.0);
        assert!(idx.qty_first.contains("A"));
        let r = idx
            .observe(&map(&[("A", dec!(200))]), &map(&[("A", dec!(5))]), t(3))
            .unwrap();
        assert_eq!(r, 0.0, "the price following is not a loss");
        assert!(idx.qty_first.is_empty() && idx.suspicions.is_empty());
        let r = idx
            .observe(&map(&[("A", dec!(200))]), &map(&[("A", dec!(5.5))]), t(4))
            .unwrap();
        assert!((r - 0.1).abs() < 1e-12);
    }

    #[test]
    fn selling_a_suspected_symbol_counts_zero_and_drops_the_suspicion() {
        // S3: the suspected asset is sold while its quote is still halved.
        let mut idx = RiskIndex::new();
        idx.observe(
            &map(&[("A", dec!(100)), ("B", dec!(100))]),
            &map(&[("A", dec!(10)), ("B", dec!(10))]),
            t(1),
        )
        .unwrap();
        idx.observe(
            &map(&[("A", dec!(100)), ("B", dec!(100))]),
            &map(&[("A", dec!(5)), ("B", dec!(10))]),
            t(2),
        )
        .unwrap();
        let r = idx
            .observe(
                &map(&[("B", dec!(100))]),
                &map(&[("A", dec!(5)), ("B", dec!(10))]),
                t(3),
            )
            .unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.level, 1.0);
        assert!(idx.suspicions.is_empty() && idx.suspects.is_empty());
    }

    #[test]
    fn owner_ratio_rebases_with_zero_return() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(100), dec!(10));
        let h = map(&[("A", dec!(100))]);
        idx.observe(&h, &map(&[("A", dec!(5))]), t(2)).unwrap();
        assert!(idx.request_ratio("A"));
        assert!(!idx.request_ratio("B"));
        let r = idx.observe(&h, &map(&[("A", dec!(4.6))]), t(3)).unwrap();
        assert_eq!(r, 0.0);
        assert_eq!(idx.anomalies[0].kind, AnomalyKind::CorporateAction);
        assert!(idx.suspicions.is_empty() && idx.ratios.is_empty());
        assert_eq!(idx.prices_usd["A"], dec!(4.6));
    }

    #[test]
    fn a_request_for_a_symbol_that_is_not_pending_is_refused() {
        let mut idx = RiskIndex::new();
        prev(&mut idx, dec!(100), dec!(10));
        assert!(!idx.request_drop("A"));
        assert!(idx.drops.is_empty());
    }

    #[test]
    fn suspicion_saved_by_the_previous_version_still_resolves() {
        let old = r#"{"level":1.0,"holdings":{"A":"100"},"prices_usd":{"A":"10"},"at":null,
            "suspicions":{"A":3}}"#;
        let mut idx: RiskIndex = serde_json::from_str(old).unwrap();
        assert!(idx.suspects.is_empty());
        // The quantity arrives with the price down 55 %: recognised against the base.
        let r = idx
            .observe(&map(&[("A", dec!(222))]), &map(&[("A", dec!(4.5))]), t(2))
            .unwrap();
        assert_eq!(r, 0.0);
        assert!(idx.suspicions.is_empty());
    }
}
