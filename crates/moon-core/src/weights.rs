use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::asset::AssetId;

pub const WEIGHT_EPS: f64 = 1e-9;

/// Target portfolio weights per asset (including `CASH`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TargetWeights(pub BTreeMap<AssetId, f64>);

impl TargetWeights {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn all_in(id: AssetId) -> Self {
        let mut w = Self::new();
        w.add(id, 1.0);
        w
    }

    /// Accumulates weight; ignores non-positive (and NaN) values.
    pub fn add(&mut self, id: AssetId, w: f64) {
        if w > 0.0 {
            *self.0.entry(id).or_insert(0.0) += w;
        }
    }

    pub fn get(&self, id: &AssetId) -> f64 {
        self.0.get(id).copied().unwrap_or(0.0)
    }

    pub fn total(&self) -> f64 {
        self.0.values().sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&AssetId, f64)> {
        self.0.iter().map(|(k, v)| (k, *v))
    }

    /// All weights finite and non-negative, total within 1e-6 of 1.
    pub fn is_valid(&self) -> bool {
        self.0.values().all(|w| w.is_finite() && *w >= 0.0) && (self.total() - 1.0).abs() <= 1e-6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_validity() {
        let mut w = TargetWeights::new();
        w.add(AssetId::new("A"), 0.5);
        w.add(AssetId::new("B"), 0.5);
        assert!(w.is_valid());
        w.add(AssetId::new("B"), 0.1);
        assert!(!w.is_valid());
        let mut n = TargetWeights::new();
        n.0.insert(AssetId::new("A"), f64::NAN);
        assert!(!n.is_valid());
    }

    #[test]
    fn add_accumulates_and_ignores_non_positive() {
        let mut w = TargetWeights::new();
        w.add(AssetId::new("A"), 0.25);
        w.add(AssetId::new("A"), 0.25);
        w.add(AssetId::new("B"), 0.0);
        w.add(AssetId::new("C"), -1.0);
        assert_eq!(w.get(&AssetId::new("A")), 0.5);
        assert_eq!(w.get(&AssetId::new("B")), 0.0);
        assert_eq!(w.iter().count(), 1);
        assert_eq!(TargetWeights::all_in(AssetId::cash()).total(), 1.0);
    }
}
