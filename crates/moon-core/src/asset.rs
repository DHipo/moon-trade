use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::types::str_enum;

/// Identifier of an asset in the universe (or `CASH`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AssetId(pub String);

impl AssetId {
    pub const CASH: &'static str = "CASH";

    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn cash() -> Self {
        Self(Self::CASH.to_string())
    }

    pub fn is_cash(&self) -> bool {
        self.0 == Self::CASH
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

str_enum! {
    pub enum Market { Bcba => "BCBA", Nyse => "NYSE", Nasdaq => "NASDAQ" }
}

str_enum! {
    pub enum Currency { Ars => "ARS", Usd => "USD" }
}

str_enum! {
    pub enum Role {
        Risky => "risky",
        Refuge => "refuge",
        Canary => "canary",
        Benchmark => "benchmark",
    }
}

fn default_lot() -> u32 {
    1
}

fn default_ratio() -> f64 {
    1.0
}

/// How an asset is executed at the broker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecSpec {
    pub symbol: String,
    pub market: Market,
    pub currency: Currency,
    #[serde(default = "default_lot")]
    pub lot_size: u32,
}

/// One asset of the universe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetSpec {
    pub id: AssetId,
    pub role: Role,
    #[serde(default)]
    pub signal_symbol: Option<String>,
    /// CEDEARs per underlying share.
    #[serde(default = "default_ratio")]
    pub cedear_ratio: f64,
    #[serde(default)]
    pub exec: Option<ExecSpec>,
}

/// Validated set of assets.
#[derive(Debug, Clone, PartialEq)]
pub struct Universe {
    assets: Vec<AssetSpec>,
}

fn invalid(msg: String) -> CoreError {
    CoreError::InvalidUniverse(msg)
}

impl Universe {
    pub fn new(assets: Vec<AssetSpec>) -> Result<Self, CoreError> {
        let mut seen = BTreeSet::new();
        let mut refuges = 0;
        let mut risky = 0;
        for a in &assets {
            let id = a.id.as_str();
            if !seen.insert(id.to_string()) {
                return Err(invalid(format!("id duplicado: {id}")));
            }
            if !a.cedear_ratio.is_finite() || a.cedear_ratio <= 0.0 {
                return Err(invalid(format!("{id}: cedear_ratio debe ser > 0")));
            }
            if let Some(e) = &a.exec
                && e.lot_size < 1
            {
                return Err(invalid(format!("{id}: lot_size debe ser >= 1")));
            }
            if a.id.is_cash() && a.role != Role::Refuge {
                return Err(invalid("CASH solo puede ser el refugio".to_string()));
            }
            match a.role {
                Role::Risky => {
                    risky += 1;
                    if a.signal_symbol.is_none() || a.exec.is_none() {
                        return Err(invalid(format!("{id}: requiere signal_symbol y exec")));
                    }
                }
                Role::Refuge => {
                    refuges += 1;
                    if a.id.is_cash() {
                        if a.exec.is_some() {
                            return Err(invalid("CASH no puede tener exec".to_string()));
                        }
                    } else if a.signal_symbol.is_none() || a.exec.is_none() {
                        return Err(invalid(format!("{id}: requiere signal_symbol y exec")));
                    }
                }
                Role::Canary | Role::Benchmark => {
                    if a.signal_symbol.is_none() {
                        return Err(invalid(format!("{id}: requiere signal_symbol")));
                    }
                }
            }
        }
        if refuges != 1 {
            return Err(invalid(format!(
                "debe haber exactamente un refugio (hay {refuges})"
            )));
        }
        if risky == 0 {
            return Err(invalid(
                "debe haber al menos un activo de riesgo".to_string(),
            ));
        }
        Ok(Self { assets })
    }

    pub fn assets(&self) -> &[AssetSpec] {
        &self.assets
    }

    pub fn risky(&self) -> Vec<&AssetSpec> {
        self.assets
            .iter()
            .filter(|a| a.role == Role::Risky)
            .collect()
    }

    pub fn refuge(&self) -> &AssetSpec {
        // Validated in `new`: exactly one refuge exists.
        self.assets
            .iter()
            .find(|a| a.role == Role::Refuge)
            .unwrap_or(&self.assets[0])
    }

    pub fn canaries(&self) -> Vec<&AssetSpec> {
        self.assets
            .iter()
            .filter(|a| a.role == Role::Canary)
            .collect()
    }

    pub fn benchmark(&self) -> Option<&AssetSpec> {
        self.assets.iter().find(|a| a.role == Role::Benchmark)
    }

    /// Risky assets plus the refuge, excluding `CASH`.
    pub fn tradable(&self) -> Vec<&AssetSpec> {
        self.assets
            .iter()
            .filter(|a| matches!(a.role, Role::Risky | Role::Refuge) && !a.id.is_cash())
            .collect()
    }

    pub fn get(&self, id: &AssetId) -> Option<&AssetSpec> {
        self.assets.iter().find(|a| &a.id == id)
    }

    pub fn by_exec_symbol(&self, symbol: &str) -> Option<&AssetSpec> {
        self.assets
            .iter()
            .find(|a| a.exec.as_ref().is_some_and(|e| e.symbol == symbol))
    }

    /// Unique signal symbols, sorted.
    pub fn signal_symbols(&self) -> Vec<String> {
        self.assets
            .iter()
            .filter_map(|a| a.signal_symbol.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::spec;

    fn risky(id: &str) -> AssetSpec {
        spec(id, Role::Risky)
    }

    #[test]
    fn universe_rejects_duplicate_ids() {
        let r = Universe::new(vec![risky("SPY"), risky("SPY"), spec("CASH", Role::Refuge)]);
        assert!(matches!(r, Err(CoreError::InvalidUniverse(_))));
    }

    #[test]
    fn universe_requires_single_refuge() {
        assert!(Universe::new(vec![risky("SPY")]).is_err());
        let two = Universe::new(vec![
            risky("SPY"),
            spec("SHY", Role::Refuge),
            spec("CASH", Role::Refuge),
        ]);
        assert!(two.is_err());
    }

    #[test]
    fn universe_risky_needs_exec() {
        let mut a = risky("SPY");
        a.exec = None;
        assert!(Universe::new(vec![a, spec("CASH", Role::Refuge)]).is_err());
    }

    #[test]
    fn universe_accepts_cash_refuge() {
        let u = Universe::new(vec![risky("SPY"), spec("CASH", Role::Refuge)]).unwrap();
        assert!(u.refuge().id.is_cash());
        assert_eq!(u.risky().len(), 1);
    }

    #[test]
    fn universe_tradable_excludes_cash() {
        let u =
            Universe::new(vec![risky("SPY"), risky("QQQ"), spec("CASH", Role::Refuge)]).unwrap();
        assert_eq!(u.tradable().len(), 2);
    }

    #[test]
    fn universe_lookups_and_symbols() {
        let u = Universe::new(vec![
            risky("SPY"),
            spec("VWO", Role::Canary),
            spec("ACWI", Role::Benchmark),
            spec("CASH", Role::Refuge),
        ])
        .unwrap();
        assert_eq!(u.canaries().len(), 1);
        assert_eq!(u.benchmark().unwrap().id.as_str(), "ACWI");
        assert!(u.get(&AssetId::new("SPY")).is_some());
        assert!(u.by_exec_symbol("SPY").is_some());
        assert!(u.by_exec_symbol("VWO").is_none());
        assert_eq!(u.signal_symbols(), vec!["ACWI", "SPY", "VWO"]);
    }

    #[test]
    fn universe_rejects_cash_non_refuge_and_bad_numbers() {
        assert!(
            Universe::new(vec![risky("CASH"), risky("SPY"), spec("SHY", Role::Refuge)]).is_err()
        );
        let mut a = risky("SPY");
        a.cedear_ratio = 0.0;
        assert!(Universe::new(vec![a, spec("CASH", Role::Refuge)]).is_err());
        let mut b = risky("SPY");
        if let Some(e) = b.exec.as_mut() {
            e.lot_size = 0;
        }
        assert!(Universe::new(vec![b, spec("CASH", Role::Refuge)]).is_err());
        let mut c = spec("CASH", Role::Refuge);
        c.exec = Some(ExecSpec {
            symbol: "X".into(),
            market: Market::Bcba,
            currency: Currency::Ars,
            lot_size: 1,
        });
        assert!(Universe::new(vec![risky("SPY"), c]).is_err());
    }

    #[test]
    fn asset_spec_parses_from_toml() {
        let s = "id = \"SPY\"\nrole = \"risky\"\nsignal_symbol = \"SPY\"\ncedear_ratio = 20.0\nexec = { symbol = \"SPY\", market = \"BCBA\", currency = \"ARS\" }";
        let a: AssetSpec = toml::from_str(s).unwrap();
        let e = a.exec.unwrap();
        assert_eq!(e.lot_size, 1);
        assert_eq!(e.market, Market::Bcba);
        assert_eq!(a.cedear_ratio, 20.0);
    }

    #[test]
    fn market_currency_round_trip() {
        for m in [Market::Bcba, Market::Nyse, Market::Nasdaq] {
            assert_eq!(m.as_str().parse::<Market>().unwrap(), m);
        }
        for c in [Currency::Ars, Currency::Usd] {
            assert_eq!(c.as_str().parse::<Currency>().unwrap(), c);
        }
        assert_eq!(Market::Bcba.as_str(), "BCBA");
    }
}
