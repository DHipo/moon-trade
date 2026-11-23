//! Antonacci dual momentum: relative (top N) plus absolute (beat the refuge) momentum.

use crate::asset::AssetId;
use crate::indicators::trailing_return;
use crate::strategy::{SignalContext, Sizing, Strategy, fill_refuge, size_selection};
use crate::weights::TargetWeights;

pub struct DualMomentum {
    pub lookback_months: usize,
    pub top_n: usize,
    pub sizing: Sizing,
}

impl Strategy for DualMomentum {
    fn name(&self) -> &'static str {
        "dual_momentum"
    }

    fn target(&self, ctx: &SignalContext<'_>) -> TargetWeights {
        let mut candidates: Vec<(AssetId, f64)> = ctx
            .universe()
            .risky()
            .iter()
            .filter_map(|s| {
                trailing_return(&ctx.monthly(&s.id), self.lookback_months)
                    .map(|r| (s.id.clone(), r))
            })
            .collect();
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let hurdle = ctx.refuge_return(self.lookback_months);
        let passing: Vec<AssetId> = candidates
            .into_iter()
            .take(self.top_n)
            .filter(|(_, r)| *r > hurdle)
            .map(|(id, _)| id)
            .collect();
        let budget = if self.top_n == 0 {
            0.0
        } else {
            passing.len() as f64 / self.top_n as f64
        };
        let mut w = size_selection(ctx, &passing, budget, self.sizing);
        fill_refuge(ctx, &mut w);
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{Role, Universe};
    use crate::bars::PriceSeries;
    use crate::testutil::{d, monthly_series, spec};
    use std::collections::BTreeMap;

    /// 13 monthly closes with the given 12-month total return.
    fn with_return(r: f64) -> PriceSeries {
        let mut c = vec![100.0; 12];
        c.push(100.0 * (1.0 + r));
        monthly_series(2023, 6, &c)
    }

    fn setup(rets: &[f64]) -> (Universe, BTreeMap<AssetId, PriceSeries>) {
        let mut specs = Vec::new();
        let mut m = BTreeMap::new();
        for (i, r) in rets.iter().enumerate() {
            let id = format!("R{i}");
            specs.push(spec(&id, Role::Risky));
            m.insert(AssetId::new(id), with_return(*r));
        }
        specs.push(spec("CASH", Role::Refuge));
        (Universe::new(specs).unwrap(), m)
    }

    fn dm(top_n: usize) -> DualMomentum {
        DualMomentum {
            lookback_months: 12,
            top_n,
            sizing: Sizing::Equal,
        }
    }

    #[test]
    fn dual_momentum_picks_top_n_and_filters_absolute() {
        let (u, m) = setup(&[0.10, 0.30, -0.05]);
        let ctx = SignalContext::new(d(2030, 1, 1), &u, &m);
        let w = dm(2).target(&ctx);
        assert_eq!(w.get(&AssetId::new("R1")), 0.5);
        assert_eq!(w.get(&AssetId::new("R0")), 0.5);
        assert_eq!(w.get(&AssetId::new("R2")), 0.0);
        assert_eq!(w.get(&AssetId::cash()), 0.0);
        let w = dm(3).target(&ctx);
        assert!((w.get(&AssetId::new("R1")) - 1.0 / 3.0).abs() < 1e-12);
        assert!((w.get(&AssetId::new("R0")) - 1.0 / 3.0).abs() < 1e-12);
        assert!((w.get(&AssetId::cash()) - 1.0 / 3.0).abs() < 1e-12);
        assert!(w.is_valid());
        assert_eq!(dm(1).name(), "dual_momentum");
    }

    #[test]
    fn dual_momentum_compares_against_refuge_asset() {
        let u = Universe::new(vec![spec("R", Role::Risky), spec("SHY", Role::Refuge)]).unwrap();
        let mut m = BTreeMap::new();
        m.insert(AssetId::new("R"), with_return(0.10));
        m.insert(AssetId::new("SHY"), with_return(0.20));
        let ctx = SignalContext::new(d(2030, 1, 1), &u, &m);
        let w = dm(1).target(&ctx);
        assert_eq!(w.get(&AssetId::new("SHY")), 1.0);
        assert_eq!(w.get(&AssetId::new("R")), 0.0);
    }
}
