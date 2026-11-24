use std::path::Path;

use anyhow::Context;
use moon_store::Store;

use crate::config::{AppConfig, load_secrets, validate_live};

/// Creates the data and log directories and the SQLite database (schema included).
pub fn run(config_path: &Path) -> anyhow::Result<()> {
    let cfg = AppConfig::load(config_path)?;
    if cfg.is_live() {
        validate_live(&cfg, &load_secrets())?;
    }
    if let Some(parent) = cfg
        .database_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("no se pudo crear {}", parent.display()))?;
    }
    std::fs::create_dir_all(&cfg.log_dir)
        .with_context(|| format!("no se pudo crear {}", cfg.log_dir.display()))?;
    Store::open(&cfg.database_path)
        .with_context(|| format!("no se pudo abrir la base {}", cfg.database_path.display()))?;
    println!(
        "Listo: base de datos en {} y logs en {}.",
        cfg.database_path.display(),
        cfg.log_dir.display()
    );
    println!("Siguiente paso: moon data sync");
    Ok(())
}
