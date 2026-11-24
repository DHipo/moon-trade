use std::collections::BTreeMap;

use anyhow::{Context, bail};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use moon_core::asset::AssetId;
use moon_core::bars::{Bar, PriceSeries};
use moon_core::time::{NEW_YORK, art_date};
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

/// Days re-downloaded on every sync so that dividend/split adjustments of recent bars are noticed.
pub const SYNC_OVERLAP_DAYS: i64 = 90;
/// Relative tolerance on `adj_close` before the stored history counts as restated.
const RESTATE_TOLERANCE: f64 = 1e-6;

/// Drops the bar of a US session that has not closed yet: bars dated on or after today in New
/// York are kept only once it is 17:00 there (and never a bar dated in the future).
pub fn drop_open_session(bars: Vec<Bar>, now: DateTime<Utc>) -> Vec<Bar> {
    let ny = now.with_timezone(&NEW_YORK);
    let today = ny.date_naive();
    let closed = ny.time() >= NaiveTime::from_hms_opt(17, 0, 0).unwrap_or(NaiveTime::MIN);
    bars.into_iter()
        .filter(|b| b.date < today || (closed && b.date == today))
        .collect()
}

/// True when a freshly downloaded bar of a date already stored carries a different `adj_close`
/// (a dividend or split restated the history).
pub fn history_restated(existing: &[Bar], fresh: &[Bar]) -> bool {
    let stored: BTreeMap<NaiveDate, f64> = existing.iter().map(|b| (b.date, b.adj_close)).collect();
    fresh.iter().any(|b| {
        stored.get(&b.date).is_some_and(|old| {
            (old - b.adj_close).abs() > RESTATE_TOLERANCE * old.abs().max(f64::MIN_POSITIVE)
        })
    })
}

/// Bars to store for one symbol: the last [`SYNC_OVERLAP_DAYS`] days (plus anything newer) when
/// the overlap matches what is stored, the full history when it does not. Bars of a session that
/// has not closed are never returned.
pub async fn fetch_update(
    yahoo: &YahooClient,
    symbol: &str,
    existing: &[Bar],
    default_from: NaiveDate,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<Vec<Bar>, moon_yahoo::YahooError> {
    let Some(last) = existing.last().map(|b| b.date) else {
        return Ok(drop_open_session(
            yahoo.daily_bars(symbol, default_from, today).await?,
            now,
        ));
    };
    let start = (last - Duration::days(SYNC_OVERLAP_DAYS)).max(default_from);
    let fresh = drop_open_session(yahoo.daily_bars(symbol, start, today).await?, now);
    if history_restated(existing, &fresh) {
        tracing::info!("{symbol}: la historia se ajustó (dividendo o split), se descarga completa");
        return Ok(drop_open_session(
            yahoo.daily_bars(symbol, default_from, today).await?,
            now,
        ));
    }
    Ok(fresh)
}

/// Downloads daily bars for every signal symbol. One failing symbol does not stop the rest.
/// With `from` set, exactly that range is downloaded (no overlap logic).
pub async fn data_sync(
    cfg: &AppConfig,
    store: &mut Store,
    yahoo: &YahooClient,
    from: Option<NaiveDate>,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> anyhow::Result<SyncReport> {
    let universe = cfg.universe()?;
    let warmup = i64::try_from(cfg.allocator()?.warmup_months()).unwrap_or(0);
    let default_from = cfg.backtest.config.start - Duration::days(warmup * 31 + 31);
    let mut report = SyncReport {
        updated: Vec::new(),
        failed: Vec::new(),
    };
    for symbol in universe.signal_symbols() {
        let outcome = async {
            let bars = match from {
                Some(f) => drop_open_session(yahoo.daily_bars(&symbol, f, today).await?, now),
                None => {
                    let existing = store.load_bars(&symbol, DataSource::Yahoo)?;
                    fetch_update(yahoo, &symbol, &existing, default_from, today, now).await?
                }
            };
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
    let now = chrono::Utc::now();
    let today = art_date(now);
    let report = data_sync(cfg, &mut store, &yahoo, from, today, now).await?;
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
