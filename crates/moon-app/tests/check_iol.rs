//! `moon check iol` against a mock IOL: the extended report, and strictly read-only behaviour.

use std::path::Path;

use chrono::{Duration, Utc};
use moon_app::commands::check::check_iol;
use moon_app::config::{AppConfig, Secrets};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn envelope(data: Value) -> Value {
    json!({"meta": {"method": "", "path": "", "status_code": 200}, "data": data, "error": null})
}

async fn mount(server: &MockServer, verb: &str, p: &str, data: Value) {
    Mock::given(method(verb))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(data)))
        .mount(server)
        .await;
}

async fn mock_iol() -> MockServer {
    mock_iol_at(None).await
}

/// Like [`mock_iol`], with `last_updated` on the BCBA quotes.
async fn mock_iol_at(last_updated: Option<&str>) -> MockServer {
    let server = MockServer::start().await;
    let expires =
        (Utc::now() + Duration::minutes(15)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    mount(
        &server,
        "POST",
        "/auth/token",
        json!({"access_token": "a", "refresh_token": "r", "expires_in": 900,
               "expires_at": expires, "token_type": "bearer"}),
    )
    .await;
    mount(
        &server,
        "GET",
        "/broker/accounts/AR/balances",
        json!({
            "ars": {"currency": "ARS",
                    "totals": {"t0": 100.0, "t1": 150.0},
                    "available": {"t0": 90.0, "t1": 120.5},
                    "committed": {"t1": 29.5}},
            "usd": {"currency": "USD", "totals": {"t1": 5.0}}
        }),
    )
    .await;
    mount(
        &server,
        "GET",
        "/broker/accounts/AR/positions",
        json!({"positions": [
            {"quantity": 10, "instrument": {"symbol": "SPY"},
             "availability": {"quantity_available": 7, "quantity_held": 3}}
        ]}),
    )
    .await;
    mount(
        &server,
        "GET",
        "/broker/accounts/activities/operations",
        json!([
            {"order_id": 1, "status": "completed"},
            {"order_id": "2", "status": "completed"},
            {"order_id": "3", "status": "cancelled"}
        ]),
    )
    .await;
    mount(
        &server,
        "GET",
        "/broker/accounts/activities",
        json!([
            {"id": "a", "activity_type": "Depósito", "net_amount": 500.0,
             "currency": "ARS", "status": "completed"},
            {"id": "b", "activity_type": "Depósito", "net_amount": 250.0,
             "currency": "ARS", "status": "completed"},
            {"id": "c", "activity_type": "Dividendo", "net_amount": 10.0,
             "currency": "USD", "status": "pending"}
        ]),
    )
    .await;
    // BCBA quotes in ARS, US quotes in USD.
    Mock::given(method("GET"))
        .and(path_regex(r"^/broker/market-data/[A-Za-z]+/quote$"))
        .and(query_param("market", "BCBA"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "symbol": "X", "market": "BCBA", "currency": "ARS",
            "prices": {"current": 12000.0}, "last_updated": last_updated
        }))))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/broker/market-data/[A-Za-z]+/quote$"))
        .and(query_param("market", "NASDAQ"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "symbol": "X", "market": "NASDAQ", "currency": "USD",
            "prices": {"current": 10.0}
        }))))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/broker/market-data/[A-Za-z]+/quote$"))
        .and(query_param("market", "NYSE"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "symbol": "X", "market": "NYSE", "currency": "USD",
            "prices": {"current": 10.0}
        }))))
        .mount(&server)
        .await;
    server
}

fn secrets() -> Secrets {
    Secrets {
        iol_username: Some("user".into()),
        iol_password: Some("pass".into()),
        telegram_token: None,
        telegram_chat_id: None,
    }
}

#[tokio::test]
async fn check_iol_prints_the_extended_report() {
    let server = mock_iol().await;
    let mut cfg = AppConfig::load(Path::new("../../moon.example.toml")).expect("example");
    cfg.iol.base_url = server.uri();
    let out = check_iol(&cfg, &secrets()).await.expect("report");

    // balances per term
    assert!(out.contains("total: T0 100.00 · T1 150.00"), "{out}");
    assert!(out.contains("disponible: T0 90.00 · T1 120.50"), "{out}");
    assert!(out.contains("comprometido:"), "{out}");
    // positions with availability
    assert!(
        out.contains("SPY: cantidad 10 · disponible 7.00 · retenida 3.00"),
        "{out}"
    );
    // distinct operation statuses and activity types with amounts
    assert!(
        out.contains("3 devueltas, total informado por IOL: no informado"),
        "{out}"
    );
    assert!(out.contains("estado «completed»: 2"), "{out}");
    assert!(out.contains("estado «cancelled»: 1"), "{out}");
    assert!(
        out.contains("tipo «Depósito», estado «completed», ARS: 2 movimiento(s), suma 750.00"),
        "{out}"
    );
    assert!(
        out.contains("tipo «Dividendo», estado «pending», USD: 1"),
        "{out}"
    );
    // FX pairs and the CCL
    assert!(out.contains("Pares del CCL implícito"), "{out}");
    assert!(out.contains("GGAL 12000"), "{out}");
    assert!(out.contains("CCL implícito: 12000"), "{out}");
}

#[tokio::test]
async fn check_iol_never_places_estimates_or_cancels_orders() {
    let server = mock_iol().await;
    let mut cfg = AppConfig::load(Path::new("../../moon.example.toml")).expect("example");
    cfg.iol.base_url = server.uri();
    check_iol(&cfg, &secrets()).await.expect("report");
    for req in server.received_requests().await.expect("requests") {
        let p = req.url.path();
        assert!(!p.starts_with("/broker/orders"), "{} {p}", req.method);
        assert!(
            req.method.as_str() == "GET" || p == "/auth/token",
            "{} {p}",
            req.method
        );
    }
}

#[tokio::test]
async fn check_iol_needs_credentials() {
    let cfg = AppConfig::load(Path::new("../../moon.example.toml")).expect("example");
    let err = check_iol(&cfg, &Secrets::default())
        .await
        .expect_err("no creds");
    assert!(err.to_string().contains("MOON_IOL_USERNAME"));
}

#[tokio::test]
async fn check_iol_shows_the_quote_time_and_freshness() {
    // Without `last_updated`: shown as absent and not fresh.
    let server = mock_iol().await;
    let mut cfg = AppConfig::load(Path::new("../../moon.example.toml")).expect("example");
    cfg.iol.base_url = server.uri();
    let out = check_iol(&cfg, &secrets()).await.expect("report");
    assert!(out.contains("last_updated crudo (ausente)"), "{out}");
    assert!(
        out.contains("NO fresca (last_updated ausente o ilegible)"),
        "{out}"
    );

    // A stamp from now in dd/mm/yyyy format (ART is UTC-3): read and fresh.
    let raw = (Utc::now() - Duration::hours(3))
        .format("%d/%m/%Y %H:%M:%S")
        .to_string();
    let server = mock_iol_at(Some(&raw)).await;
    cfg.iol.base_url = server.uri();
    let out = check_iol(&cfg, &secrets()).await.expect("report");
    assert!(
        out.contains(&format!("last_updated crudo «{raw}»")),
        "{out}"
    );
    assert!(out.contains("ART"), "{out}");
    assert!(out.contains("· fresca"), "{out}");

    // An old stamp is read but not fresh; an unreadable one is flagged.
    let server = mock_iol_at(Some("2020-01-02T10:00:00")).await;
    cfg.iol.base_url = server.uri();
    let out = check_iol(&cfg, &secrets()).await.expect("report");
    assert!(out.contains("2020-01-02 10:00:00 ART"), "{out}");
    assert!(out.contains("NO fresca (tiene"), "{out}");
    let server = mock_iol_at(Some("anteayer")).await;
    cfg.iol.base_url = server.uri();
    let out = check_iol(&cfg, &secrets()).await.expect("report");
    assert!(out.contains("interpretado no se pudo interpretar"), "{out}");
}
