//! Typed `moon.toml` configuration, strict validation and secrets from the environment.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use chrono::{NaiveDate, NaiveTime, Weekday};
use moon_broker::FxPair;
use moon_core::allocator::{AllocationConfig, Allocator};
use moon_core::asset::{AssetSpec, Universe};
use moon_core::backtest::BacktestConfig;
use moon_core::fees::FeeModel;
use moon_core::risk::RiskLimits;
use moon_core::types::{OrderType, Term};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradingMode {
    Paper,
    LiveConfirm,
    LiveAuto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteSourceKind {
    Iol,
    Yahoo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionConfig {
    pub slippage_rate: Decimal,
    pub limit_tolerance: Decimal,
    /// Emergency sells are limit orders this far below the last price (fraction, 0.03 = 3 %).
    #[serde(default = "default_emergency_limit_discount")]
    pub emergency_limit_discount: Decimal,
    pub price_tick: Decimal,
    pub settlement_term: Term,
    pub order_type: OrderType,
    pub quote_source: QuoteSourceKind,
    pub approval_timeout_minutes: u32,
    pub fill_timeout_minutes: u32,
    pub poll_interval_seconds: u32,
}

fn default_emergency_limit_discount() -> Decimal {
    Decimal::new(3, 2)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxConfig {
    pub pairs: Vec<FxPair>,
    pub min_pairs: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleConfig {
    pub market_open: NaiveTime,
    pub market_close: NaiveTime,
    pub monthly_time: NaiveTime,
    pub daily_close_time: NaiveTime,
    pub risk_check_every_minutes: u32,
    pub weekly_report_weekday: Weekday,
    pub weekly_report_time: NaiveTime,
    #[serde(default)]
    pub holidays: Vec<NaiveDate>,
}

/// Backtest settings; amounts (capital, fees) are in USD.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestSettings {
    #[serde(flatten)]
    pub config: BacktestConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperConfig {
    pub initial_cash_ars: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IolConfig {
    pub base_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelegramConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub mode: TradingMode,
    pub database_path: PathBuf,
    pub log_dir: PathBuf,
    pub assets: Vec<AssetSpec>,
    pub allocation: AllocationConfig,
    pub risk: RiskLimits,
    /// Live/paper fees, amounts in ARS.
    pub fees: FeeModel,
    pub execution: ExecutionConfig,
    pub fx: FxConfig,
    pub schedule: ScheduleConfig,
    /// Backtest settings, amounts in USD.
    pub backtest: BacktestSettings,
    pub paper: PaperConfig,
    pub live: LiveConfig,
    pub iol: IolConfig,
    pub telegram: TelegramConfig,
}

/// Credentials read from the environment. `Debug` never prints values.
#[derive(Clone, Default)]
pub struct Secrets {
    pub iol_username: Option<String>,
    pub iol_password: Option<String>,
    pub telegram_token: Option<String>,
    pub telegram_chat_id: Option<i64>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mask = |present: bool| if present { "***" } else { "None" };
        f.debug_struct("Secrets")
            .field("iol_username", &mask(self.iol_username.is_some()))
            .field("iol_password", &mask(self.iol_password.is_some()))
            .field("telegram_token", &mask(self.telegram_token.is_some()))
            .field("telegram_chat_id", &mask(self.telegram_chat_id.is_some()))
            .finish()
    }
}

#[derive(Serialize)]
struct HashPayload<'a> {
    assets: &'a [AssetSpec],
    allocation: &'a AllocationConfig,
    risk: &'a RiskLimits,
    backtest: &'a BacktestSettings,
}

impl AppConfig {
    pub fn load(path: &Path) -> anyhow::Result<AppConfig> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("no se pudo leer la configuración {}", path.display()))?;
        let cfg: AppConfig = toml::from_str(&text)
            .with_context(|| format!("configuración inválida en {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.universe()?;
        self.allocator()?;
        self.risk.validate()?;
        self.fees.validate().context("[fees]")?;
        self.backtest
            .config
            .fees
            .validate()
            .context("[backtest.fees]")?;

        let max_slip = Decimal::new(5, 2);
        let in_range = |x: Decimal| x >= Decimal::ZERO && x <= max_slip;
        if !in_range(self.execution.slippage_rate) {
            bail!("execution.slippage_rate debe estar entre 0 y 0.05");
        }
        if !in_range(self.execution.limit_tolerance) {
            bail!("execution.limit_tolerance debe estar entre 0 y 0.05");
        }
        if self.execution.emergency_limit_discount < Decimal::ZERO
            || self.execution.emergency_limit_discount > Decimal::new(2, 1)
        {
            bail!("execution.emergency_limit_discount debe estar entre 0 y 0.2");
        }
        if self.execution.price_tick <= Decimal::ZERO {
            bail!("execution.price_tick debe ser mayor que 0");
        }
        if self.fx.min_pairs < 1 || self.fx.min_pairs > self.fx.pairs.len() {
            bail!(
                "fx.min_pairs debe estar entre 1 y la cantidad de pares configurados ({})",
                self.fx.pairs.len()
            );
        }
        if let Some(p) = self.fx.pairs.iter().find(|p| p.ratio <= Decimal::ZERO) {
            bail!("el ratio del par {} debe ser mayor que 0", p.local_symbol);
        }
        let s = &self.schedule;
        if !(s.market_open < s.monthly_time && s.monthly_time < s.market_close) {
            bail!("el horario debe cumplir market_open < monthly_time < market_close");
        }
        if s.risk_check_every_minutes < 1 {
            bail!("schedule.risk_check_every_minutes debe ser al menos 1");
        }
        if self.backtest.config.initial_capital <= Decimal::ZERO {
            bail!("backtest.initial_capital debe ser mayor que 0");
        }
        if self.mode != TradingMode::Paper && !self.live.enabled {
            bail!("El modo live requiere [live] enabled = true (doble confirmación).");
        }
        Ok(())
    }

    pub fn universe(&self) -> anyhow::Result<Universe> {
        Ok(Universe::new(self.assets.clone())?)
    }

    pub fn allocator(&self) -> anyhow::Result<Allocator> {
        Ok(Allocator::new(&self.allocation, self.risk.max_weight)?)
    }

    /// SHA-256 (hex) of the JSON of the settings that define a backtest.
    pub fn config_hash(&self) -> String {
        let digest = Sha256::digest(self.hash_json().as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The JSON that is hashed and registered with each backtest run.
    pub fn hash_json(&self) -> String {
        let payload = HashPayload {
            assets: &self.assets,
            allocation: &self.allocation,
            risk: &self.risk,
            backtest: &self.backtest,
        };
        serde_json::to_string(&payload).unwrap_or_default()
    }

    pub fn is_live(&self) -> bool {
        self.mode != TradingMode::Paper
    }
}

/// Loads `.env` (if any) and reads credentials from the environment.
pub fn load_secrets() -> Secrets {
    dotenvy::dotenv().ok();
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Secrets {
        iol_username: get("MOON_IOL_USERNAME"),
        iol_password: get("MOON_IOL_PASSWORD"),
        telegram_token: get("MOON_TELEGRAM_TOKEN"),
        telegram_chat_id: get("MOON_TELEGRAM_CHAT_ID").and_then(|v| v.trim().parse().ok()),
    }
}

pub fn validate_live(cfg: &AppConfig, secrets: &Secrets) -> anyhow::Result<()> {
    if !cfg.is_live() {
        return Ok(());
    }
    if !cfg.live.enabled {
        bail!("El modo live requiere [live] enabled = true (doble confirmación).");
    }
    if secrets.iol_username.is_none() || secrets.iol_password.is_none() {
        bail!(
            "El modo live requiere MOON_IOL_USERNAME y MOON_IOL_PASSWORD en el entorno o en .env."
        );
    }
    Ok(())
}
