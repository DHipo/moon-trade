//! `moon risk accept`: the owner accepts a pending suspected ratio change as a real move.
//!
//! Purely local: it records the acceptance in the database; the service applies it at its next
//! risk check. It never calls the broker.

use moon_core::types::ExecMode;
use moon_store::Store;

use crate::commands::orders::exec_mode;
use crate::config::AppConfig;
use crate::engine::request_ratio_accept;

/// Records the acceptance and returns the Spanish confirmation text.
pub fn accept_text(store: &mut Store, mode: ExecMode, symbol: &str) -> anyhow::Result<String> {
    let symbol = request_ratio_accept(store, mode, symbol)?;
    Ok(format!(
        "Aceptado: el movimiento de {symbol} se contará como real en el próximo control de riesgo \
         (cada 15 minutos con el servicio corriendo). Si el freno por caída lo confirma, \
         vende las posiciones de riesgo."
    ))
}

/// CLI entry of `moon risk accept <symbol>`.
pub fn run_accept(cfg: &AppConfig, symbol: &str) -> anyhow::Result<()> {
    let mut store = Store::open(&cfg.database_path)?;
    println!("{}", accept_text(&mut store, exec_mode(cfg), symbol)?);
    Ok(())
}
