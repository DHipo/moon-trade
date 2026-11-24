use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};
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

/// 23:00 UTC of `day`: past 17:00 in New York.
fn after_close(day: chrono::NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&day.and_hms_opt(23, 0, 0).expect("time"))
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
    let report = data_sync(&cfg, &mut store, &yahoo, None, today, after_close(today))
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

fn chart(timestamps: &[i64], adj: &[f64]) -> String {
    let n = adj.len();
    let list = |v: &[f64]| {
        v.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        r#"{{"chart":{{"result":[{{"meta":{{"gmtoffset":0}},"timestamp":[{}],
"indicators":{{"quote":[{{"open":[{o}],"high":[{o}],"low":[{o}],"close":[{o}],"volume":[{v}]}}],
"adjclose":[{{"adjclose":[{}]}}]}}}}],"error":null}}}}"#,
        timestamps
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
        list(adj),
        o = list(&vec![1.0; n]),
        v = list(&vec![10.0; n]),
    )
}

fn one_symbol_config() -> AppConfig {
    let mut cfg = small_config();
    cfg.assets = vec![spec("AAA", Role::Risky), spec("CASH", Role::Refuge)];
    cfg.allocation.canary_threshold = None;
    cfg.validate().expect("valid");
    cfg
}

#[tokio::test]
async fn sync_redownloads_full_history_when_the_overlap_was_restated() {
    let cfg = one_symbol_config();
    let server = MockServer::start().await;
    // 2024-01-02 and 2024-01-03: the overlap comes back with a restated adj_close.
    let ts = [1_704_205_800, 1_704_292_200];
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(chart(&ts, &[0.5, 1.0]), "application/json"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // The full re-download carries one more (older) bar, so we can tell it was used.
    let full_ts = [1_704_119_400, 1_704_205_800, 1_704_292_200];
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(chart(&full_ts, &[0.25, 0.5, 1.0]), "application/json"),
        )
        .mount(&server)
        .await;
    let yahoo =
        YahooClient::with_base_url(server.uri()).with_retry_delay(std::time::Duration::ZERO);
    let mut store = Store::open_in_memory().expect("store");
    let old = daily_series(d(2024, 1, 2), &[1.0, 2.0]);
    store
        .upsert_bars("AAA", DataSource::Yahoo, old.bars())
        .expect("seed");
    let today = d(2024, 1, 10);
    let report = data_sync(&cfg, &mut store, &yahoo, None, today, after_close(today))
        .await
        .expect("sync");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let bars = store.load_bars("AAA", DataSource::Yahoo).expect("bars");
    assert_eq!(bars.len(), 3, "full history replaced the stored one");
    assert!((bars[1].adj_close - 0.5).abs() < 1e-9);
}

#[tokio::test]
async fn sync_keeps_the_overlap_window_when_nothing_was_restated() {
    let cfg = one_symbol_config();
    let server = MockServer::start().await;
    let ts = [1_704_205_800, 1_704_292_200];
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(chart(&ts, &[1.0, 2.0]), "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let yahoo =
        YahooClient::with_base_url(server.uri()).with_retry_delay(std::time::Duration::ZERO);
    let mut store = Store::open_in_memory().expect("store");
    let old = daily_series(d(2024, 1, 2), &[1.0, 2.0]);
    store
        .upsert_bars("AAA", DataSource::Yahoo, old.bars())
        .expect("seed");
    let today = d(2024, 1, 10);
    data_sync(&cfg, &mut store, &yahoo, None, today, after_close(today))
        .await
        .expect("sync");
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn sync_never_stores_the_bar_of_the_current_us_session() {
    let cfg = one_symbol_config();
    let server = MockServer::start().await;
    // 2024-01-09 and 2024-01-10 (the current session).
    let ts = [1_704_810_600, 1_704_897_000];
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(chart(&ts, &[5.0, 6.0]), "application/json"),
        )
        .mount(&server)
        .await;
    let yahoo =
        YahooClient::with_base_url(server.uri()).with_retry_delay(std::time::Duration::ZERO);
    let today = d(2024, 1, 10);
    // 15:00 UTC = 10:00 in New York: the 2024-01-10 session is still open.
    let during = Utc.from_utc_datetime(&today.and_hms_opt(15, 0, 0).expect("time"));
    let mut store = Store::open_in_memory().expect("store");
    data_sync(&cfg, &mut store, &yahoo, None, today, during)
        .await
        .expect("sync");
    let bars = store.load_bars("AAA", DataSource::Yahoo).expect("bars");
    assert_eq!(bars.last().map(|b| b.date), Some(d(2024, 1, 9)));
    // After the close the same bar is accepted.
    data_sync(&cfg, &mut store, &yahoo, None, today, after_close(today))
        .await
        .expect("sync");
    let bars = store.load_bars("AAA", DataSource::Yahoo).expect("bars");
    assert_eq!(bars.last().map(|b| b.date), Some(d(2024, 1, 10)));
}
