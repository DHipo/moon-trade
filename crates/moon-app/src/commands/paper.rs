use anyhow::bail;
use rust_decimal::Decimal;

use crate::config::{AppConfig, Secrets};

fn ensure_paper(cfg: &AppConfig) -> anyhow::Result<()> {
    if cfg.is_live() {
        bail!("Solo disponible en modo paper.");
    }
    Ok(())
}

pub async fn run_deposit(
    cfg: AppConfig,
    secrets: &Secrets,
    amount_ars: Decimal,
) -> anyhow::Result<()> {
    ensure_paper(&cfg)?;
    let (engine, _) = super::build_engine(cfg, secrets)?;
    engine.paper_deposit(amount_ars).await?;
    println!("Depósito paper registrado: ARS {}.", amount_ars.round_dp(2));
    Ok(())
}

pub async fn run_reset(cfg: AppConfig, secrets: &Secrets) -> anyhow::Result<()> {
    ensure_paper(&cfg)?;
    let (engine, _) = super::build_engine(cfg, secrets)?;
    engine.paper_reset().await?;
    println!("Cuenta paper reiniciada.");
    Ok(())
}
