//! Risk rules: weight caps and target validation.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::asset::{AssetId, Role, Universe};
use crate::error::CoreError;
use crate::rebalance::PlannedOrder;
use crate::types::Side;
use crate::weights::TargetWeights;

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum RiskViolation {
    #[error("activo fuera del universo: {0}")]
    UnknownAsset(AssetId),
    #[error("activo no operable: {0}")]
    NonTradableAsset(AssetId),
    #[error("pesos inválidos: {0}")]
    InvalidWeights(String),
    #[error("{asset} supera el tope: {weight:.4} > {cap:.4}")]
    WeightAboveCap {
        asset: AssetId,
        weight: f64,
        cap: f64,
    },
    #[error("orden de {asset} demasiado grande: {value} > {max}")]
    OrderTooLarge {
        asset: AssetId,
        value: Decimal,
        max: Decimal,
    },
    #[error("demasiadas órdenes hoy: {count} > {max}")]
    TooManyOrders { count: u32, max: u32 },
    #[error("una salida de emergencia solo puede vender: {0}")]
    EmergencyBuy(AssetId),
}

/// Cuts every non-refuge weight above `max_weight` and moves the excess to the refuge.
pub fn apply_weight_cap(
    weights: &TargetWeights,
    refuge: &AssetId,
    max_weight: f64,
) -> TargetWeights {
    let mut out = TargetWeights::new();
    let mut excess = 0.0;
    for (id, w) in weights.iter() {
        if id == refuge {
            continue;
        }
        if w > max_weight {
            excess += w - max_weight;
            out.add(id.clone(), max_weight);
        } else {
            out.add(id.clone(), w);
        }
    }
    out.add(refuge.clone(), weights.get(refuge) + excess);
    out
}

/// Validates that targets only use tradable assets, form a valid distribution and respect the cap.
pub fn check_targets(
    weights: &TargetWeights,
    universe: &Universe,
    max_weight: f64,
) -> Result<(), RiskViolation> {
    for (id, _) in weights.iter() {
        let spec = universe
            .get(id)
            .ok_or_else(|| RiskViolation::UnknownAsset(id.clone()))?;
        if !matches!(spec.role, Role::Risky | Role::Refuge) {
            return Err(RiskViolation::NonTradableAsset(id.clone()));
        }
    }
    if !weights.is_valid() {
        return Err(RiskViolation::InvalidWeights(format!(
            "total {:.6}",
            weights.total()
        )));
    }
    for (id, w) in weights.iter() {
        let is_refuge = universe.get(id).is_some_and(|s| s.role == Role::Refuge);
        if !is_refuge && w > max_weight + 1e-9 {
            return Err(RiskViolation::WeightAboveCap {
                asset: id.clone(),
                weight: w,
                cap: max_weight,
            });
        }
    }
    Ok(())
}

/// Configurable hard limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_weight: f64,
    pub max_drawdown: f64,
    pub rebalance_band: f64,
    pub min_trade_usd: Decimal,
    pub max_order_usd: Decimal,
    pub max_orders_per_day: u32,
    pub cash_buffer: f64,
    #[serde(default)]
    pub max_equity_usd: Option<Decimal>,
}

impl RiskLimits {
    pub fn validate(&self) -> Result<(), CoreError> {
        let bad = |m: &str| Err(CoreError::InvalidConfig(m.to_string()));
        let in_range = |x: f64, lo: f64, hi: f64, lo_incl: bool, hi_incl: bool| {
            x.is_finite() && (x > lo || (lo_incl && x == lo)) && (x < hi || (hi_incl && x == hi))
        };
        if !in_range(self.max_weight, 0.0, 1.0, false, true) {
            return bad("max_weight debe estar en (0, 1]");
        }
        if !in_range(self.max_drawdown, 0.0, 1.0, false, false) {
            return bad("max_drawdown debe estar en (0, 1)");
        }
        if !in_range(self.rebalance_band, 0.0, 0.5, true, false) {
            return bad("rebalance_band debe estar en [0, 0.5)");
        }
        if !in_range(self.cash_buffer, 0.0, 0.2, true, true) {
            return bad("cash_buffer debe estar en [0, 0.2]");
        }
        if self.min_trade_usd < Decimal::ZERO {
            return bad("min_trade_usd debe ser >= 0");
        }
        if self.max_order_usd <= Decimal::ZERO {
            return bad("max_order_usd debe ser > 0");
        }
        if self.max_orders_per_day < 1 {
            return bad("max_orders_per_day debe ser >= 1");
        }
        if self.max_equity_usd.is_some_and(|m| m <= Decimal::ZERO) {
            return bad("max_equity_usd debe ser > 0");
        }
        Ok(())
    }
}

/// Order-level checks. Emergency exits only verify that every order is a sell.
pub fn check_orders(
    orders: &[PlannedOrder],
    max_order_value: Decimal,
    max_orders_per_day: u32,
    already_today: u32,
    emergency: bool,
) -> Result<(), RiskViolation> {
    if emergency {
        return match orders.iter().find(|o| o.side != Side::Sell) {
            Some(o) => Err(RiskViolation::EmergencyBuy(o.asset.clone())),
            None => Ok(()),
        };
    }
    if let Some(o) = orders.iter().find(|o| o.est_value > max_order_value) {
        return Err(RiskViolation::OrderTooLarge {
            asset: o.asset.clone(),
            value: o.est_value,
            max: max_order_value,
        });
    }
    let count = already_today.saturating_add(u32::try_from(orders.len()).unwrap_or(u32::MAX));
    if count > max_orders_per_day {
        return Err(RiskViolation::TooManyOrders {
            count,
            max: max_orders_per_day,
        });
    }
    Ok(())
}

/// Drawdown breaker state. It works on a level series (normally `RiskIndex::level`, which
/// measures the invested part in USD and ignores cash movements). `peak_nav` is a running
/// maximum; only the first monthly decision after a trip resets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DrawdownState {
    pub peak_nav: f64,
    pub tripped: bool,
    pub tripped_on: Option<NaiveDate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawdownSignal {
    Normal,
    Tripped,
    StillTripped,
}

impl DrawdownState {
    pub fn observe(&mut self, nav: f64, today: NaiveDate, max_drawdown: f64) -> DrawdownSignal {
        if self.tripped {
            return DrawdownSignal::StillTripped;
        }
        if nav > self.peak_nav {
            self.peak_nav = nav;
        }
        if self.drawdown(nav) >= max_drawdown {
            self.tripped = true;
            self.tripped_on = Some(today);
            DrawdownSignal::Tripped
        } else {
            DrawdownSignal::Normal
        }
    }

    pub fn drawdown(&self, nav: f64) -> f64 {
        if self.peak_nav <= 0.0 {
            0.0
        } else {
            (1.0 - nav / self.peak_nav).max(0.0)
        }
    }

    /// Monthly decision. When the breaker is tripped, the decision clears it and re-bases the
    /// peak at `level`; when it is not tripped the peak is left untouched, so a slow decline
    /// across several months still accumulates. Returns whether a reset happened.
    pub fn monthly_decision(&mut self, level: f64) -> bool {
        if !self.tripped {
            return false;
        }
        self.reset(level);
        true
    }

    /// Unconditional reset: peak = level, tripped = false.
    pub fn reset(&mut self, nav: f64) {
        self.peak_nav = nav;
        self.tripped = false;
        self.tripped_on = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::spec;

    fn universe() -> Universe {
        Universe::new(vec![
            spec("A", Role::Risky),
            spec("V", Role::Canary),
            spec("CASH", Role::Refuge),
        ])
        .unwrap()
    }

    #[test]
    fn cap_moves_excess_to_refuge() {
        let mut w = TargetWeights::new();
        w.add(AssetId::new("A"), 0.5);
        w.add(AssetId::cash(), 0.5);
        let c = apply_weight_cap(&w, &AssetId::cash(), 0.2);
        assert!((c.get(&AssetId::new("A")) - 0.2).abs() < 1e-12);
        assert!((c.get(&AssetId::cash()) - 0.8).abs() < 1e-12);
    }

    #[test]
    fn check_targets_rejects_unknown_and_canary() {
        let u = universe();
        let w = TargetWeights::all_in(AssetId::new("Z"));
        assert_eq!(
            check_targets(&w, &u, 1.0),
            Err(RiskViolation::UnknownAsset(AssetId::new("Z")))
        );
        let w = TargetWeights::all_in(AssetId::new("V"));
        assert_eq!(
            check_targets(&w, &u, 1.0),
            Err(RiskViolation::NonTradableAsset(AssetId::new("V")))
        );
        let mut w = TargetWeights::new();
        w.add(AssetId::new("A"), 0.5);
        assert!(matches!(
            check_targets(&w, &u, 1.0),
            Err(RiskViolation::InvalidWeights(_))
        ));
        let w = TargetWeights::all_in(AssetId::new("A"));
        assert!(matches!(
            check_targets(&w, &u, 0.5),
            Err(RiskViolation::WeightAboveCap { .. })
        ));
        assert!(check_targets(&w, &u, 1.0).is_ok());
    }

    fn order(asset: &str, side: Side, value: Decimal) -> PlannedOrder {
        PlannedOrder {
            asset: AssetId::new(asset),
            side,
            qty: Decimal::ONE,
            ref_price: value,
            exec_price: value,
            est_value: value,
            est_fees: Decimal::ZERO,
        }
    }

    #[test]
    fn check_orders_limits() {
        use rust_decimal_macros::dec;
        let big = [order("A", Side::Buy, dec!(600))];
        assert!(matches!(
            check_orders(&big, dec!(500), 10, 0, false),
            Err(RiskViolation::OrderTooLarge { .. })
        ));
        let two = [
            order("A", Side::Buy, dec!(100)),
            order("B", Side::Sell, dec!(100)),
        ];
        assert_eq!(
            check_orders(&two, dec!(500), 3, 2, false),
            Err(RiskViolation::TooManyOrders { count: 4, max: 3 })
        );
        assert!(check_orders(&two, dec!(500), 3, 1, false).is_ok());
        assert_eq!(
            check_orders(&two, dec!(500), 3, 0, true),
            Err(RiskViolation::EmergencyBuy(AssetId::new("A")))
        );
        let sells = [order("B", Side::Sell, dec!(100000))];
        assert!(check_orders(&sells, dec!(500), 1, 5, true).is_ok());
    }

    #[test]
    fn risk_limits_validate_ranges() {
        use rust_decimal_macros::dec;
        let ok = RiskLimits {
            max_weight: 0.5,
            max_drawdown: 0.15,
            rebalance_band: 0.05,
            min_trade_usd: dec!(10),
            max_order_usd: dec!(1000),
            max_orders_per_day: 10,
            cash_buffer: 0.01,
            max_equity_usd: None,
        };
        assert!(ok.validate().is_ok());
        let cases = vec![
            RiskLimits {
                max_weight: 0.0,
                ..ok.clone()
            },
            RiskLimits {
                max_weight: 1.1,
                ..ok.clone()
            },
            RiskLimits {
                max_drawdown: 0.0,
                ..ok.clone()
            },
            RiskLimits {
                max_drawdown: 1.0,
                ..ok.clone()
            },
            RiskLimits {
                rebalance_band: 0.5,
                ..ok.clone()
            },
            RiskLimits {
                rebalance_band: -0.1,
                ..ok.clone()
            },
            RiskLimits {
                cash_buffer: 0.21,
                ..ok.clone()
            },
            RiskLimits {
                min_trade_usd: dec!(-1),
                ..ok.clone()
            },
            RiskLimits {
                max_order_usd: dec!(0),
                ..ok.clone()
            },
            RiskLimits {
                max_orders_per_day: 0,
                ..ok.clone()
            },
            RiskLimits {
                max_equity_usd: Some(dec!(0)),
                ..ok.clone()
            },
            RiskLimits {
                max_weight: f64::NAN,
                ..ok.clone()
            },
        ];
        for c in cases {
            assert!(
                matches!(c.validate(), Err(CoreError::InvalidConfig(_))),
                "{c:?}"
            );
        }
        let edge = RiskLimits {
            max_weight: 1.0,
            rebalance_band: 0.0,
            cash_buffer: 0.2,
            max_equity_usd: Some(dec!(1)),
            ..ok
        };
        assert!(edge.validate().is_ok());
    }

    #[test]
    fn drawdown_trips_once() {
        let day = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let mut s = DrawdownState {
            peak_nav: 1.0,
            ..Default::default()
        };
        assert_eq!(s.observe(0.9, day, 0.15), DrawdownSignal::Normal);
        assert_eq!(s.observe(0.84, day, 0.15), DrawdownSignal::Tripped);
        assert_eq!(s.tripped_on, Some(day));
        assert_eq!(s.observe(0.84, day, 0.15), DrawdownSignal::StillTripped);
        s.reset(0.84);
        assert_eq!(s.observe(0.84, day, 0.15), DrawdownSignal::Normal);
        assert_eq!(s.peak_nav, 0.84);
        assert_eq!(DrawdownState::default().drawdown(0.5), 0.0);
    }

    #[test]
    fn monthly_decision_only_resets_after_a_trip() {
        let day = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let mut s = DrawdownState {
            peak_nav: 1.0,
            ..Default::default()
        };
        s.observe(0.92, day, 0.15);
        assert!(!s.monthly_decision(0.92));
        assert_eq!(s.peak_nav, 1.0);
        s.observe(0.84, day, 0.15);
        assert!(s.tripped);
        assert!(s.monthly_decision(0.84));
        assert!(!s.tripped);
        assert_eq!(s.peak_nav, 0.84);
        assert_eq!(s.tripped_on, None);
    }
}
