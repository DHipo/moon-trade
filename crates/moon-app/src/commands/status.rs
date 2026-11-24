use crate::config::{AppConfig, Secrets};

pub async fn run_status(cfg: AppConfig, secrets: &Secrets) -> anyhow::Result<()> {
    let (engine, _) = super::build_engine(cfg, secrets)?;
    println!("{}", engine.status_text().await?);
    Ok(())
}
