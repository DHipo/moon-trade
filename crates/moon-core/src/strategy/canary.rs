//! Keller/Keuning canary crash protection and strategy combination.

use serde::{Deserialize, Serialize};

use crate::indicators::momentum_13612w;
use crate::strategy::SignalContext;
use crate::weights::TargetWeights;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryReport {
    pub bad: usize,
    pub total: usize,
    pub defensive_fraction: f64,
}

pub struct CanaryOverlay {
    pub threshold: usize,
}

impl CanaryOverlay {
    /// Shifts a fraction of every risky weight to the refuge depending on how many
    /// canaries have negative 13612W momentum.
    pub fn apply(
        &self,
        ctx: &SignalContext<'_>,
        weights: TargetWeights,
    ) -> (TargetWeights, CanaryReport) {
        let mut bad = 0;
        let mut total = 0;
        for c in ctx.universe().canaries() {
            if let Some(m) = momentum_13612w(&ctx.monthly(&c.id)) {
                total += 1;
                if m < 0.0 {
                    bad += 1;
                }
            }
        }
        let f = if total == 0 || self.threshold == 0 {
            0.0
        } else {
            bad.min(self.threshold) as f64 / self.threshold as f64
        };
        let refuge = ctx.universe().refuge().id.clone();
        let mut out = TargetWeights::new();
        let mut removed = 0.0;
        for (id, w) in weights.iter() {
            if *id == refuge {
                out.add(id.clone(), w);
            } else {
                out.add(id.clone(), w * (1.0 - f));
                removed += w * f;
            }
        }
        out.add(refuge, removed);
        (
            out,
            CanaryReport {
                bad,
                total,
                defensive_fraction: f,
            },
        )
    }
}

/// Equal-weight average of several targets; empty input gives empty weights.
pub fn combine(targets: &[TargetWeights]) -> TargetWeights {
    let mut out = TargetWeights::new();
    if targets.is_empty() {
        return out;
    }
    let n = targets.len() as f64;
    for t in targets {
        for (id, w) in t.iter() {
            out.add(id.clone(), w / n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AssetId, Role, Universe};
    use crate::bars::PriceSeries;
    use crate::testutil::{d, monthly_series, spec};
    use std::collections::BTreeMap;

    #[test]
    fn canary_half_defensive() {
        let u = Universe::new(vec![
            spec("A", Role::Risky),
            spec("V1", Role::Canary),
            spec("V2", Role::Canary),
            spec("CASH", Role::Refuge),
        ])
        .unwrap();
        let up: Vec<f64> = (0..13).map(|i| 100.0 * 1.01_f64.powi(i)).collect();
        let down: Vec<f64> = (0..13).map(|i| 100.0 * 0.99_f64.powi(i)).collect();
        let mut m: BTreeMap<AssetId, PriceSeries> = BTreeMap::new();
        m.insert(AssetId::new("V1"), monthly_series(2023, 6, &up));
        m.insert(AssetId::new("V2"), monthly_series(2023, 6, &down));
        let ctx = SignalContext::new(d(2030, 1, 1), &u, &m);
        let mut w = TargetWeights::new();
        w.add(AssetId::new("A"), 0.8);
        w.add(AssetId::cash(), 0.2);
        let (out, rep) = CanaryOverlay { threshold: 2 }.apply(&ctx, w);
        assert_eq!(
            rep,
            CanaryReport {
                bad: 1,
                total: 2,
                defensive_fraction: 0.5
            }
        );
        assert!((out.get(&AssetId::new("A")) - 0.4).abs() < 1e-12);
        assert!((out.get(&AssetId::cash()) - 0.6).abs() < 1e-12);
    }

    #[test]
    fn combine_averages() {
        assert!(combine(&[]).0.is_empty());
        let a = TargetWeights::all_in(AssetId::new("A"));
        let c = TargetWeights::all_in(AssetId::cash());
        let w = combine(&[a, c]);
        assert_eq!(w.get(&AssetId::new("A")), 0.5);
        assert_eq!(w.get(&AssetId::cash()), 0.5);
    }
}
