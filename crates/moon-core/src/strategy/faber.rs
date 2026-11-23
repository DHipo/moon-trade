//! Faber timing: hold each risky asset only while its price is above its N-month SMA.

use crate::asset::AssetId;
use crate::indicators::sma;
use crate::strategy::{SignalContext, Sizing, Strategy, fill_refuge, size_selection};
use crate::weights::TargetWeights;

pub struct FaberSma {
    pub months: usize,
    pub sizing: Sizing,
}

impl Strategy for FaberSma {
    fn name(&self) -> &'static str {
        "faber_sma"
    }

    fn target(&self, ctx: &SignalContext<'_>) -> TargetWeights {
        let risky = ctx.universe().risky();
        let n = risky.len();
        let mut selected: Vec<AssetId> = Vec::new();
        for spec in &risky {
            let closes: Vec<f64> = ctx.monthly(&spec.id).iter().map(|(_, c)| *c).collect();
            if let (Some(avg), Some(last)) = (sma(&closes, self.months), closes.last())
                && *last > avg
            {
                selected.push(spec.id.clone());
            }
        }
        let budget = if n == 0 {
            0.0
        } else {
            selected.len() as f64 / n as f64
        };
        let mut w = size_selection(ctx, &selected, budget, self.sizing);
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

    fn setup(a: &[f64], b: &[f64]) -> (Universe, BTreeMap<AssetId, PriceSeries>) {
        let u = Universe::new(vec![
            spec("A", Role::Risky),
            spec("B", Role::Risky),
            spec("CASH", Role::Refuge),
        ])
        .unwrap();
        let mut m = BTreeMap::new();
        m.insert(AssetId::new("A"), monthly_series(2023, 1, a));
        m.insert(AssetId::new("B"), monthly_series(2023, 1, b));
        (u, m)
    }

    fn strat() -> FaberSma {
        FaberSma {
            months: 10,
            sizing: Sizing::Equal,
        }
    }

    #[test]
    fn faber_above_sma_gets_share() {
        let rising: Vec<f64> = (0..11).map(|i| 100.0 + i as f64).collect();
        let falling: Vec<f64> = (0..11).map(|i| 100.0 - i as f64).collect();
        let (u, m) = setup(&rising, &falling);
        let ctx = SignalContext::new(d(2030, 1, 1), &u, &m);
        let w = strat().target(&ctx);
        assert_eq!(w.get(&AssetId::new("A")), 0.5);
        assert_eq!(w.get(&AssetId::new("B")), 0.0);
        assert_eq!(w.get(&AssetId::cash()), 0.5);
        assert_eq!(strat().name(), "faber_sma");
    }

    #[test]
    fn faber_insufficient_history_goes_to_refuge() {
        let short: Vec<f64> = (0..5).map(|i| 100.0 + i as f64).collect();
        let (u, m) = setup(&short, &short);
        let ctx = SignalContext::new(d(2030, 1, 1), &u, &m);
        let w = strat().target(&ctx);
        assert_eq!(w.get(&AssetId::cash()), 1.0);
        assert_eq!(w.iter().count(), 1);
    }
}
