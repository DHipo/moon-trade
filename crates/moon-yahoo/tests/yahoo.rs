use std::time::Duration;

use chrono::NaiveDate;
use moon_yahoo::{YahooClient, YahooError};
use serde_json::Value;
use wiremock::matchers::{header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SPY: &str = include_str!("fixtures/chart_spy.json");
const ERR: &str = include_str!("fixtures/chart_error.json");

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).expect("valid date")
}

fn client(server: &MockServer) -> YahooClient {
    YahooClient::with_base_url(server.uri()).with_retry_delay(Duration::ZERO)
}

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json")
}

#[tokio::test]
async fn parses_bars_and_skips_nulls() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v8/finance/chart/SPY"))
        .respond_with(json(SPY))
        .mount(&server)
        .await;
    let bars = client(&server)
        .daily_bars("SPY", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect("bars");
    assert_eq!(bars.len(), 4);
    let dates: Vec<_> = bars.iter().map(|b| b.date).collect();
    assert_eq!(
        dates,
        vec![d(2024, 1, 2), d(2024, 1, 3), d(2024, 1, 5), d(2024, 1, 6)]
    );
    assert_eq!(bars[0].open, 100.0);
    assert_eq!(bars[0].close, 100.5);
    assert_eq!(bars[0].adj_close, 99.5);
    assert_eq!(bars[0].volume, 1000.0);
    assert_eq!(bars[2].adj_close, 102.5);
}

#[tokio::test]
async fn uses_close_when_adjclose_missing() {
    let mut v: Value = serde_json::from_str(SPY).expect("json");
    v["chart"]["result"][0]["indicators"]
        .as_object_mut()
        .expect("obj")
        .remove("adjclose");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(&v.to_string()))
        .mount(&server)
        .await;
    let bars = client(&server)
        .daily_bars("SPY", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect("bars");
    assert_eq!(bars.len(), 4);
    assert_eq!(bars[0].adj_close, bars[0].close);
}

#[tokio::test]
async fn sends_expected_query() {
    let server = MockServer::start().await;
    // 2024-01-02 00:00 UTC and 2024-01-07 00:00 UTC.
    Mock::given(method("GET"))
        .and(path("/v8/finance/chart/SPY"))
        .and(query_param("interval", "1d"))
        .and(query_param("period1", "1704153600"))
        .and(query_param("period2", "1704585600"))
        .and(query_param("events", "div,splits"))
        .and(query_param("includeAdjustedClose", "true"))
        .and(header(
            "user-agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) moon-trade",
        ))
        .respond_with(json(SPY))
        .expect(1)
        .mount(&server)
        .await;
    client(&server)
        .daily_bars("SPY", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect("bars");
}

#[tokio::test]
async fn encodes_special_symbols() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/v8/finance/chart/.*GSPC$"))
        .respond_with(json(SPY))
        .expect(1)
        .mount(&server)
        .await;
    let bars = client(&server)
        .daily_bars("^GSPC", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect("bars");
    assert_eq!(bars.len(), 4);
}

#[tokio::test]
async fn api_error_maps_to_api_variant() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(ERR))
        .mount(&server)
        .await;
    let err = client(&server)
        .daily_bars("NOPE", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect_err("api error");
    match err {
        YahooError::Api { code, description } => {
            assert_eq!(code, "Not Found");
            assert_eq!(description, "No data found");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn empty_result_is_no_data() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(r#"{"chart":{"result":[],"error":null}}"#))
        .mount(&server)
        .await;
    let err = client(&server)
        .daily_bars("SPY", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect_err("no data");
    assert!(matches!(err, YahooError::NoData(_)), "{err:?}");
}

#[tokio::test]
async fn retries_on_500_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(json(SPY))
        .with_priority(2)
        .mount(&server)
        .await;
    let bars = client(&server)
        .daily_bars("SPY", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect("bars after retry");
    assert_eq!(bars.len(), 4);
}

#[tokio::test]
async fn gives_up_after_three_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .expect(3)
        .mount(&server)
        .await;
    let err = client(&server)
        .daily_bars("SPY", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect_err("fails");
    assert!(matches!(err, YahooError::Status(429)), "{err:?}");
}

#[tokio::test]
async fn no_retry_on_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let err = client(&server)
        .daily_bars("SPY", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect_err("404");
    assert!(matches!(err, YahooError::Status(404)), "{err:?}");
}

#[tokio::test]
async fn latest_quote_reads_meta() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v8/finance/chart/SPY"))
        .and(query_param("range", "5d"))
        .and(query_param("interval", "1d"))
        .respond_with(json(SPY))
        .expect(1)
        .mount(&server)
        .await;
    let q = client(&server).latest_quote("SPY").await.expect("quote");
    assert_eq!(q.symbol, "SPY");
    assert_eq!(q.price, 475.5);
    assert_eq!(q.currency, "USD");
    assert_eq!(q.at.timestamp(), 1_704_832_800);
}

#[tokio::test]
async fn slow_server_times_out_after_retries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(SPY).set_delay(Duration::from_secs(1)))
        .expect(3)
        .mount(&server)
        .await;
    let err = client(&server)
        .with_timeout(Duration::from_millis(200))
        .daily_bars("SPY", d(2024, 1, 2), d(2024, 1, 6))
        .await
        .expect_err("timeout");
    match err {
        YahooError::Http(e) => assert!(e.is_timeout(), "{e:?}"),
        other => panic!("unexpected {other:?}"),
    }
}
