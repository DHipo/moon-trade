//! Risk rules: weight caps and target validation.

use rust_decimal::Decimal;

use crate::asset::{AssetId, Role, Universe};
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
}
