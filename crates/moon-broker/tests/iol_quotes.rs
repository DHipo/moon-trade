use std::sync::Arc;

use chrono::{DateTime, Duration, TimeZone, Utc};
use moon_broker::{IolQuotes, QuoteSource};
use moon_core::asset::Market;
use moon_core::time::FixedClock;
use moon_core::types::Term;
use moon_iol::{IolClient, IolCredentials};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 23, 15, 0, 0)
        .single()
        .expect("time")
}

fn envelope(data: Value) -> Value {
    json!({"meta": {"method": "", "path": "", "status_code": 200}, "data": data, "error": null})
}

/// Returns the server too: it must outlive the quote source.
async fn quotes_with(prices: Value, last_updated: Option<&str>) -> (MockServer, IolQuotes) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .and(body_partial_json(json!({"grant_type": "password"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "access_token": "t1", "refresh_token": "r1", "expires_in": 900,
            "expires_at": (t0() + Duration::minutes(15))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "token_type": "bearer"
        }))))
        .mount(&server)
        .await;
    let mut body = json!({
        "symbol": "SPY", "market": "BCBA", "settlement_term": "T1", "currency": "ARS",
        "prices": prices, "lot_size": 1
    });
    if let Some(l) = last_updated {
        body["last_updated"] = json!(l);
    }
    Mock::given(method("GET"))
        .and(path("/broker/market-data/SPY/quote"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(body)))
        .mount(&server)
        .await;
    let client = Arc::new(IolClient::new(
        server.uri(),
        IolCredentials {
            username: "user".into(),
            password: "dummy-test-pw".into(),
        },
        Arc::new(FixedClock::new(t0())),
    ));
    (server, IolQuotes::new(client, Term::T1))
}

#[tokio::test]
async fn quote_time_comes_from_last_updated() {
    let (_server, quotes) =
        quotes_with(json!({"current": 100.5}), Some("2026-11-23T12:00:00")).await;
    let q = quotes.quote("SPY", Market::Bcba).await.expect("quote");
    assert_eq!(q.at, t0(), "naive values are ART");
    assert!(q.is_fresh(t0() + Duration::minutes(10)));
    assert!(!q.is_fresh(t0() + Duration::minutes(40)));
}

#[tokio::test]
async fn quote_without_a_readable_time_is_never_fresh() {
    for stamp in [None, Some("no es una fecha")] {
        let (_server, quotes) = quotes_with(json!({"current": 100.5}), stamp).await;
        let q = quotes.quote("SPY", Market::Bcba).await.expect("quote");
        assert_eq!(q.at, DateTime::<Utc>::UNIX_EPOCH);
        assert!(!q.is_fresh(t0()));
    }
}

#[tokio::test]
async fn a_close_fallback_is_never_fresh() {
    let (_server, quotes) =
        quotes_with(json!({"previous_close": 99.0}), Some("2026-11-23T12:00:00")).await;
    let q = quotes.quote("SPY", Market::Bcba).await.expect("quote");
    assert_eq!(q.at, DateTime::<Utc>::UNIX_EPOCH);
}
