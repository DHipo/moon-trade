use std::path::Path;

use chrono::Utc;
use moon_app::commands::backtest::run_backtest_cmd;
use moon_app::commands::data::{data_sync, load_series};
use moon_app::config::AppConfig;
use moon_core::allocator::{AllocationConfig, SizingKind, StrategyConfig};
use moon_core::asset::Role;
use moon_core::testutil::{d, daily_series, spec};
use moon_core::types::DataSource;
use moon_store::Store;
use moon_yahoo::YahooClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn small_config() -> AppConfig {
    let mut cfg = AppConfig::load(Path::new("../../moon.example.toml")).expect("example");
    cfg.assets = vec![
        spec("AAA", Role::Risky),
        spec("BBB", Role::Risky),
        spec("CASH", Role::Refuge),
        spec("CCC", Role::Canary),
        spec("BENCH", Role::Benchmark),
    ];
    cfg.allocation = AllocationConfig {
        strategies: vec![StrategyConfig::FaberSma { months: 6 }],
        sizing: SizingKind::Equal,
        vol_lookback_days: 63,
        canary_threshold: Some(1),
    };
    cfg.backtest.config.start = d(2018, 1, 1);
    cfg.backtest.config.holdout_start = None;
    cfg.validate().expect("small config valid");
    cfg
}

fn closes(phase: f64) -> Vec<f64> {
    (0..1300)
        .map(|i| 100.0 + i as f64 * 0.03 + ((i as f64) * 0.05 + phase).sin() * 6.0)
        .collect()
}

fn seed(store: &mut Store, cfg: &AppConfig) {
    for (i, sym) in cfg
        .universe()
        .expect("universe")
        .signal_symbols()
        .iter()
        .enumerate()
    {
        let series = daily_series(d(2018, 1, 1), &closes(i as f64));
        store
            .upsert_bars(sym, DataSource::Yahoo, series.bars())
            .expect("upsert");
    }
}

#[test]
fn backtest_cmd_runs_on_synthetic_data_and_registers() {
    let cfg = small_config();
    let mut store = Store::open_in_memory().expect("store");
    seed(&mut store, &cfg);
    let out = run_backtest_cmd(&cfg, &mut store, false, Utc::now()).expect("backtest");
    assert!(out.run_id > 0);
    assert_eq!(out.distinct_configs, 1);
    let again = run_backtest_cmd(&cfg, &mut store, false, Utc::now()).expect("backtest");
    assert!(again.run_id > out.run_id);
    assert_eq!(again.distinct_configs, 1);
    assert_eq!(store.backtest_runs().expect("runs").len(), 2);
}

#[test]
fn load_series_requires_data() {
    let cfg = small_config();
    let store = Store::open_in_memory().expect("store");
    assert!(load_series(&cfg, &store).is_err());
}

const CHART: &str = r#"{"chart":{"result":[{"meta":{"gmtoffset":0},"timestamp":[1704205800,1704292200],
"indicators":{"quote":[{"open":[1.0,2.0],"high":[1.0,2.0],"low":[1.0,2.0],"close":[1.0,2.0],"volume":[10,20]}],
"adjclose":[{"adjclose":[1.0,2.0]}]}}],"error":null}}"#;

#[tokio::test]
async fn data_sync_reports_partial_failure() {
    let cfg = small_config();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v8/finance/chart/BBB"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(CHART, "application/json"))
        .mount(&server)
        .await;
    let yahoo =
        YahooClient::with_base_url(server.uri()).with_retry_delay(std::time::Duration::ZERO);
    let mut store = Store::open_in_memory().expect("store");
    let today = d(2024, 1, 10);
    let report = data_sync(&cfg, &mut store, &yahoo, None, today)
        .await
        .expect("sync");
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "BBB");
    assert_eq!(report.updated.len(), 3);
    assert_eq!(
        store
            .load_bars("AAA", DataSource::Yahoo)
            .expect("bars")
            .len(),
        2
    );
}
