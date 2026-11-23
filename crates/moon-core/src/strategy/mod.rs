//! Allocation strategies and shared signal helpers.

pub mod canary;
pub mod dual_momentum;
pub mod faber;

use std::collections::BTreeMap;

use chrono::NaiveDate;

use crate::asset::{AssetId, Universe};
use crate::bars::{Bar, PriceSeries, monthly_closes};
use crate::indicators::{annualized_volatility, trailing_return};
use crate::weights::{TargetWeights, WEIGHT_EPS};

/// Read-only view of the market data a strategy may use, cut at `as_of` (no look-ahead).
pub struct SignalContext<'a> {
    as_of: NaiveDate,
    universe: &'a Universe,
    series: &'a BTreeMap<AssetId, PriceSeries>,
}

impl<'a> SignalContext<'a> {
    pub fn new(
        as_of: NaiveDate,
        universe: &'a Universe,
        series: &'a BTreeMap<AssetId, PriceSeries>,
    ) -> Self {
        Self {
            as_of,
            universe,
            series,
        }
    }

    pub fn as_of(&self) -> NaiveDate {
        self.as_of
    }

    pub fn universe(&self) -> &'a Universe {
        self.universe
    }

    /// Daily bars up to and including `as_of`; empty if the asset has no series.
    pub fn daily(&self, id: &AssetId) -> &'a [Bar] {
        match self.series.get(id) {
            Some(s) => s.up_to(self.as_of),
            None => &[],
        }
    }

    pub fn monthly(&self, id: &AssetId) -> Vec<(NaiveDate, f64)> {
        monthly_closes(self.daily(id))
    }

    /// Trailing return of the refuge asset; `CASH` or missing data count as 0.
    pub fn refuge_return(&self, months: usize) -> f64 {
        let refuge = &self.universe.refuge().id;
        if refuge.is_cash() {
            return 0.0;
        }
        trailing_return(&self.monthly(refuge), months).unwrap_or(0.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sizing {
    Equal,
    InverseVol { lookback_days: usize },
}

pub trait Strategy: Send + Sync {
    fn name(&self) -> &'static str;
    /// Risky + refuge weights, total 1.
    fn target(&self, ctx: &SignalContext<'_>) -> TargetWeights;
}

/// Splits `budget` across the selected assets.
pub fn size_selection(
    ctx: &SignalContext<'_>,
    selected: &[AssetId],
    budget: f64,
    sizing: Sizing,
) -> TargetWeights {
    let mut out = TargetWeights::new();
    if selected.is_empty() {
        return out;
    }
    if let Sizing::InverseVol { lookback_days } = sizing {
        let vols: Vec<Option<f64>> = selected
            .iter()
            .map(|id| annualized_volatility(ctx.daily(id), lookback_days))
            .collect();
        if vols
            .iter()
            .all(|v| v.is_some_and(|x| x.is_finite() && x > 0.0))
        {
            let inv: Vec<f64> = vols.iter().map(|v| 1.0 / v.unwrap_or(1.0)).collect();
            let total: f64 = inv.iter().sum();
            for (id, i) in selected.iter().zip(inv) {
                out.add(id.clone(), budget * i / total);
            }
            return out;
        }
    }
    let equal = budget / selected.len() as f64;
    for id in selected {
        out.add(id.clone(), equal);
    }
    out
}

/// Adds whatever is left of the unit budget to the refuge.
pub(crate) fn fill_refuge(ctx: &SignalContext<'_>, weights: &mut TargetWeights) {
    let rest = 1.0 - weights.total();
    if rest > WEIGHT_EPS {
        weights.add(ctx.universe().refuge().id.clone(), rest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::Role;
    use crate::testutil::{d, daily_series, spec};

    fn series_with_returns(rets: &[f64]) -> PriceSeries {
        let mut closes = vec![100.0];
        for r in rets {
            let last = closes[closes.len() - 1];
            closes.push(last * (1.0 + r));
        }
        daily_series(d(2024, 1, 1), &closes)
    }

    #[test]
    fn inverse_vol_favors_low_vol() {
        let u = Universe::new(vec![
            spec("A", Role::Risky),
            spec("B", Role::Risky),
            spec("CASH", Role::Refuge),
        ])
        .unwrap();
        let alt =
            |a: f64| -> Vec<f64> { (0..40).map(|i| if i % 2 == 0 { a } else { -a }).collect() };
        let mut m = BTreeMap::new();
        m.insert(AssetId::new("A"), series_with_returns(&alt(0.01)));
        m.insert(AssetId::new("B"), series_with_returns(&alt(0.02)));
        let ctx = SignalContext::new(d(2030, 1, 1), &u, &m);
        let sel = [AssetId::new("A"), AssetId::new("B")];
        let w = size_selection(&ctx, &sel, 0.9, Sizing::InverseVol { lookback_days: 30 });
        assert!((w.get(&AssetId::new("A")) - 0.6).abs() < 1e-9);
        assert!((w.get(&AssetId::new("B")) - 0.3).abs() < 1e-9);
        // Missing history falls back to equal weights.
        let w = size_selection(&ctx, &sel, 0.9, Sizing::InverseVol { lookback_days: 500 });
        assert!((w.get(&AssetId::new("A")) - 0.45).abs() < 1e-9);
        assert!(size_selection(&ctx, &[], 1.0, Sizing::Equal).0.is_empty());
    }
}
