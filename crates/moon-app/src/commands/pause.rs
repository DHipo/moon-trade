use moon_store::Store;

use crate::config::{AppConfig, Secrets};
use crate::engine::STATE_PAUSED;

/// Sets the pause flag directly in the store (no broker needed) and notifies.
pub async fn run_set_paused(
    cfg: &AppConfig,
    secrets: &Secrets,
    paused: bool,
) -> anyhow::Result<()> {
    let mut store = Store::open(&cfg.database_path)?;
    store.set_state(STATE_PAUSED, &paused)?;
    drop(store);
    let (notifier, _) = super::build_notifier(cfg, secrets);
    let text = if paused {
        "⏸️ Bot en pausa: no se rebalancea. Las salidas de emergencia siguen activas."
    } else {
        "▶️ Bot reanudado."
    };
    notifier.notify(text).await;
    println!("{text}");
    Ok(())
}
