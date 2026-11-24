use chrono::{DateTime, Utc};
use moon_core::backtest::{BacktestInput, BacktestResult, run_backtest};
use moon_store::{BacktestRun, Store};

use super::data::load_series;
use crate::config::AppConfig;
use crate::report::format_metrics_table;

pub struct BacktestOutcome {
    pub result: BacktestResult,
    pub run_id: i64,
    pub distinct_configs: u64,
}

/// Runs the backtest and registers it in the anti-overfitting registry.
pub fn run_backtest_cmd(
    cfg: &AppConfig,
    store: &mut Store,
    include_holdout: bool,
    now: DateTime<Utc>,
) -> anyhow::Result<BacktestOutcome> {
    let universe = cfg.universe()?;
    let allocator = cfg.allocator()?;
    let series = load_series(cfg, store)?;
    let input = BacktestInput {
        config: &cfg.backtest.config,
        risk: &cfg.risk,
        universe: &universe,
        allocator: &allocator,
        series: &series,
        include_holdout,
    };
    let result = run_backtest(&input)?;
    let run = BacktestRun {
        id: None,
        created_at: now,
        config_hash: cfg.config_hash(),
        config: serde_json::from_str(&cfg.hash_json())?,
        start: result.start,
        end: result.end,
        include_holdout,
        metrics: serde_json::json!({
            "strategy": &result.metrics,
            "benchmark": &result.benchmark,
        }),
    };
    let run_id = store.register_backtest(&run)?;
    let distinct_configs = store.count_distinct_backtest_configs()?;
    Ok(BacktestOutcome {
        result,
        run_id,
        distinct_configs,
    })
}

pub fn run_backtest_command(
    cfg: &AppConfig,
    include_holdout: bool,
    json: bool,
) -> anyhow::Result<()> {
    let mut store = Store::open(&cfg.database_path)?;
    let out = run_backtest_cmd(cfg, &mut store, include_holdout, Utc::now())?;
    if json {
        let doc = serde_json::json!({
            "run_id": out.run_id,
            "distinct_configs": out.distinct_configs,
            "include_holdout": include_holdout,
            "metrics": &out.result.metrics,
            "benchmark": &out.result.benchmark,
        });
        println!("{}", serde_json::to_string_pretty(&doc)?);
        if include_holdout {
            eprintln!("{HOLDOUT_WARNING}");
        }
        return Ok(());
    }
    println!(
        "{}",
        format_metrics_table(&out.result.metrics, out.result.benchmark.as_ref())
    );
    println!(
        "Configuraciones distintas probadas: {}",
        out.distinct_configs
    );
    if include_holdout {
        println!("{HOLDOUT_WARNING}");
    }
    Ok(())
}

const HOLDOUT_WARNING: &str =
    "⚠️ Incluiste el período virgen: este resultado ya no sirve como validación final.";

pub fn run_backtest_runs(cfg: &AppConfig) -> anyhow::Result<()> {
    let store = Store::open(&cfg.database_path)?;
    let runs = store.backtest_runs()?;
    if runs.is_empty() {
        println!("Todavía no hay backtests registrados.");
        return Ok(());
    }
    for r in &runs {
        let cagr = r
            .metrics
            .pointer("/strategy/cagr")
            .and_then(|v| v.as_f64())
            .map_or_else(|| "n/d".to_string(), |c| format!("{:.2}%", c * 100.0));
        println!(
            "#{} {} cfg {} {} a {} CAGR {}{}",
            r.id.unwrap_or_default(),
            r.created_at.format("%Y-%m-%d %H:%M"),
            r.config_hash.chars().take(8).collect::<String>(),
            r.start,
            r.end,
            cagr,
            if r.include_holdout {
                " (con período virgen)"
            } else {
                ""
            }
        );
    }
    println!(
        "Configuraciones distintas probadas: {}",
        store.count_distinct_backtest_configs()?
    );
    Ok(())
}
