use std::collections::BTreeMap;

use anyhow::{Context, bail};
use chrono::{Duration, NaiveDate};
use moon_core::asset::AssetId;
use moon_core::bars::PriceSeries;
use moon_core::time::art_date;
use moon_core::types::DataSource;
use moon_store::Store;
use moon_yahoo::YahooClient;

use crate::config::AppConfig;

#[derive(Debug, Clone, PartialEq)]
pub struct SyncReport {
    /// Symbol and number of bars stored.
    pub updated: Vec<(String, usize)>,
    /// Symbol and error message.
    pub failed: Vec<(String, String)>,
}

/// Downloads daily bars for every signal symbol. One failing symbol does not stop the rest.
pub async fn data_sync(
    cfg: &AppConfig,
    store: &mut Store,
    yahoo: &YahooClient,
    from: Option<NaiveDate>,
    today: NaiveDate,
) -> anyhow::Result<SyncReport> {
    let universe = cfg.universe()?;
    let warmup = i64::try_from(cfg.allocator()?.warmup_months()).unwrap_or(0);
    let default_from = cfg.backtest.config.start - Duration::days(warmup * 31 + 31);
    let mut report = SyncReport {
        updated: Vec::new(),
        failed: Vec::new(),
    };
    for symbol in universe.signal_symbols() {
        let start = match from {
            Some(f) => f,
            None => store
                .last_bar_date(&symbol, DataSource::Yahoo)?
                .map_or(default_from, |d| d + Duration::days(1)),
        };
        if start > today {
            report.updated.push((symbol, 0));
            continue;
        }
        let outcome = async {
            let bars = yahoo.daily_bars(&symbol, start, today).await?;
            anyhow::Ok(store.upsert_bars(&symbol, DataSource::Yahoo, &bars)?)
        }
        .await;
        match outcome {
            Ok(n) => report.updated.push((symbol, n)),
            Err(e) => report.failed.push((symbol, e.to_string())),
        }
    }
    Ok(report)
}

/// Loads the stored Yahoo history of every asset that has a signal symbol, keyed by asset id.
pub fn load_series(
    cfg: &AppConfig,
    store: &Store,
) -> anyhow::Result<BTreeMap<AssetId, PriceSeries>> {
    let mut out = BTreeMap::new();
    for asset in &cfg.assets {
        let Some(symbol) = &asset.signal_symbol else {
            continue;
        };
        let bars = store.load_bars(symbol, DataSource::Yahoo)?;
        if bars.is_empty() {
            bail!("faltan datos de {symbol}: corré `moon data sync`");
        }
        let series =
            PriceSeries::new(bars).with_context(|| format!("serie inválida para {symbol}"))?;
        out.insert(asset.id.clone(), series);
    }
    Ok(out)
}

pub async fn run_sync(cfg: &AppConfig, from: Option<NaiveDate>) -> anyhow::Result<()> {
    let mut store = Store::open(&cfg.database_path)?;
    let yahoo = YahooClient::new();
    let today = art_date(chrono::Utc::now());
    let report = data_sync(cfg, &mut store, &yahoo, from, today).await?;
    for (symbol, n) in &report.updated {
        println!("{symbol}: {n} barras");
    }
    for (symbol, err) in &report.failed {
        println!("{symbol}: ERROR {err}");
    }
    if !report.failed.is_empty() {
        bail!(
            "falló la descarga de {} símbolo(s); reintentá con `moon data sync`",
            report.failed.len()
        );
    }
    Ok(())
}
