//! Combines strategies, the canary overlay and the weight cap into one decision.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::asset::Universe;
use crate::error::CoreError;
use crate::risk::apply_weight_cap;
use crate::strategy::canary::{CanaryOverlay, CanaryReport, combine};
use crate::strategy::dual_momentum::DualMomentum;
use crate::strategy::faber::FaberSma;
use crate::strategy::{SignalContext, Sizing, Strategy};
use crate::weights::TargetWeights;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizingKind {
    #[default]
    Equal,
    InverseVol,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StrategyConfig {
    FaberSma {
        months: usize,
    },
    DualMomentum {
        lookback_months: usize,
        top_n: usize,
    },
}

fn default_vol_lookback() -> usize {
    63
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationConfig {
    pub strategies: Vec<StrategyConfig>,
    #[serde(default)]
    pub sizing: SizingKind,
    #[serde(default = "default_vol_lookback")]
    pub vol_lookback_days: usize,
    #[serde(default)]
    pub canary_threshold: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyOutput {
    pub name: String,
    pub weights: TargetWeights,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationDecision {
    pub as_of: NaiveDate,
    pub per_strategy: Vec<StrategyOutput>,
    pub combined: TargetWeights,
    pub canary: Option<CanaryReport>,
    pub final_weights: TargetWeights,
}

impl AllocationDecision {
    /// Total weight held outside the refuge.
    pub fn risky_weight(&self, universe: &Universe) -> f64 {
        let refuge = &universe.refuge().id;
        self.final_weights
            .iter()
            .filter(|(id, _)| *id != refuge)
            .map(|(_, w)| w)
            .sum()
    }
}

pub struct Allocator {
    strategies: Vec<Box<dyn Strategy>>,
    canary: Option<CanaryOverlay>,
    max_weight: f64,
    warmup: usize,
}

fn bad(msg: &str) -> CoreError {
    CoreError::InvalidConfig(msg.to_string())
}

impl Allocator {
    pub fn new(cfg: &AllocationConfig, max_weight: f64) -> Result<Self, CoreError> {
        if cfg.strategies.is_empty() {
            return Err(bad("se requiere al menos una estrategia"));
        }
        if !(max_weight > 0.0 && max_weight <= 1.0) {
            return Err(bad("max_weight debe estar en (0, 1]"));
        }
        if cfg.canary_threshold == Some(0) {
            return Err(bad("canary_threshold debe ser >= 1"));
        }
        let sizing = match cfg.sizing {
            SizingKind::Equal => Sizing::Equal,
            SizingKind::InverseVol => Sizing::InverseVol {
                lookback_days: cfg.vol_lookback_days,
            },
        };
        let mut strategies: Vec<Box<dyn Strategy>> = Vec::new();
        let mut warmup = 0;
        for s in &cfg.strategies {
            match *s {
                StrategyConfig::FaberSma { months } => {
                    if months == 0 {
                        return Err(bad("months debe ser >= 1"));
                    }
                    warmup = warmup.max(months);
                    strategies.push(Box::new(FaberSma { months, sizing }));
                }
                StrategyConfig::DualMomentum {
                    lookback_months,
                    top_n,
                } => {
                    if lookback_months == 0 || top_n == 0 {
                        return Err(bad("lookback_months y top_n deben ser >= 1"));
                    }
                    warmup = warmup.max(lookback_months);
                    strategies.push(Box::new(DualMomentum {
                        lookback_months,
                        top_n,
                        sizing,
                    }));
                }
            }
        }
        let canary = cfg
            .canary_threshold
            .map(|threshold| CanaryOverlay { threshold });
        if canary.is_some() {
            warmup = warmup.max(12);
        }
        Ok(Self {
            strategies,
            canary,
            max_weight,
            warmup: warmup + 1,
        })
    }

    /// Monthly closes needed before every signal is available.
    pub fn warmup_months(&self) -> usize {
        self.warmup
    }

    pub fn allocate(&self, ctx: &SignalContext<'_>) -> AllocationDecision {
        let per_strategy: Vec<StrategyOutput> = self
            .strategies
            .iter()
            .map(|s| StrategyOutput {
                name: s.name().to_string(),
                weights: s.target(ctx),
            })
            .collect();
        let all: Vec<TargetWeights> = per_strategy.iter().map(|o| o.weights.clone()).collect();
        let combined = combine(&all);
        let (after_canary, canary) = match &self.canary {
            Some(c) if !ctx.universe().canaries().is_empty() => {
                let (w, r) = c.apply(ctx, combined.clone());
                (w, Some(r))
            }
            _ => (combined.clone(), None),
        };
        let final_weights =
            apply_weight_cap(&after_canary, &ctx.universe().refuge().id, self.max_weight);
        AllocationDecision {
            as_of: ctx.as_of(),
            per_strategy,
            combined,
            canary,
            final_weights,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{AssetId, Role};
    use crate::bars::{Bar, PriceSeries};
    use crate::risk::check_targets;
    use crate::testutil::{d, monthly_series, spec};
    use std::collections::BTreeMap;

    fn config() -> AllocationConfig {
        AllocationConfig {
            strategies: vec![
                StrategyConfig::FaberSma { months: 10 },
                StrategyConfig::DualMomentum {
                    lookback_months: 12,
                    top_n: 2,
                },
            ],
            sizing: SizingKind::Equal,
            vol_lookback_days: 63,
            canary_threshold: Some(2),
        }
    }

    fn world() -> (Universe, BTreeMap<AssetId, PriceSeries>) {
        let u = Universe::new(vec![
            spec("A", Role::Risky),
            spec("B", Role::Risky),
            spec("V1", Role::Canary),
            spec("V2", Role::Canary),
            spec("CASH", Role::Refuge),
        ])
        .unwrap();
        let up: Vec<f64> = (0..13).map(|i| 100.0 * 1.02_f64.powi(i)).collect();
        let down: Vec<f64> = (0..13).map(|i| 100.0 * 0.98_f64.powi(i)).collect();
        let mut m = BTreeMap::new();
        m.insert(AssetId::new("A"), monthly_series(2023, 6, &up));
        m.insert(AssetId::new("B"), monthly_series(2023, 6, &down));
        m.insert(AssetId::new("V1"), monthly_series(2023, 6, &up));
        m.insert(AssetId::new("V2"), monthly_series(2023, 6, &down));
        (u, m)
    }

    #[test]
    fn allocator_output_is_valid() {
        let (u, m) = world();
        let a = Allocator::new(&config(), 0.6).unwrap();
        assert_eq!(a.warmup_months(), 13);
        let dec = a.allocate(&SignalContext::new(d(2030, 1, 1), &u, &m));
        assert!(dec.final_weights.is_valid());
        assert!(check_targets(&dec.final_weights, &u, 0.6).is_ok());
        assert_eq!(dec.per_strategy.len(), 2);
        assert_eq!(dec.canary.as_ref().map(|c| c.bad), Some(1));
        assert!(dec.risky_weight(&u) <= 0.6 + 1e-9);
    }

    #[test]
    fn no_look_ahead() {
        let (u, m) = world();
        let a = Allocator::new(&config(), 1.0).unwrap();
        let as_of = d(2024, 6, 28);
        let before = a.allocate(&SignalContext::new(as_of, &u, &m));
        let mut m2 = BTreeMap::new();
        for (id, s) in &m {
            let mut bars: Vec<Bar> = s.bars().to_vec();
            let extra: Vec<Bar> = (1..=40)
                .map(|i| {
                    let mut b = bars[bars.len() - 1].clone();
                    b.date = as_of + chrono::Duration::days(i);
                    b.close *= 10.0;
                    b.adj_close *= 10.0;
                    b
                })
                .collect();
            bars.extend(extra);
            m2.insert(id.clone(), PriceSeries::new(bars).unwrap());
        }
        let after = a.allocate(&SignalContext::new(as_of, &u, &m2));
        assert_eq!(before, after);
    }

    #[test]
    fn rejects_bad_config() {
        let mut c = config();
        c.strategies.clear();
        assert!(Allocator::new(&c, 1.0).is_err());
        let mut c = config();
        c.canary_threshold = Some(0);
        assert!(Allocator::new(&c, 1.0).is_err());
        let mut c = config();
        c.strategies = vec![StrategyConfig::FaberSma { months: 0 }];
        assert!(Allocator::new(&c, 1.0).is_err());
        c.strategies = vec![StrategyConfig::DualMomentum {
            lookback_months: 12,
            top_n: 0,
        }];
        assert!(Allocator::new(&c, 1.0).is_err());
        assert!(Allocator::new(&config(), 0.0).is_err());
        assert!(Allocator::new(&config(), 1.5).is_err());
    }

    #[test]
    fn allocation_config_parses_from_toml() {
        let s = r#"
sizing = "inverse_vol"
canary_threshold = 2

[[strategies]]
kind = "faber_sma"
months = 10

[[strategies]]
kind = "dual_momentum"
lookback_months = 12
top_n = 3
"#;
        let c: AllocationConfig = toml::from_str(s).unwrap();
        assert_eq!(c.strategies.len(), 2);
        assert_eq!(c.sizing, SizingKind::InverseVol);
        assert_eq!(c.vol_lookback_days, 63);
        assert_eq!(c.canary_threshold, Some(2));
        assert_eq!(
            c.strategies[1],
            StrategyConfig::DualMomentum {
                lookback_months: 12,
                top_n: 3
            }
        );
    }
}
