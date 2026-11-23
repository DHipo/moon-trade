use std::sync::Arc;

use chrono::{Duration, NaiveDate, TimeZone, Utc};
use moon_core::asset::Market;
use moon_core::time::FixedClock;
use moon_core::types::{OrderType, Side, Term};
use moon_iol::{
    Country, IolBar, IolClient, IolCredentials, IolError, IolOrderRequest, OperationsQuery,
};
use serde_json::{Value, json};
use uuid::Uuid;
use wiremock::matchers::{
    body_json, body_partial_json, header, header_exists, method, path, query_param,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PASSWORD: &str = "pw-secret-123";

fn creds() -> IolCredentials {
    IolCredentials {
        username: "user".into(),
        password: PASSWORD.into(),
    }
}

fn t0() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 23, 15, 0, 0).unwrap()
}

fn client(server: &MockServer) -> (IolClient, Arc<FixedClock>) {
    let clock = Arc::new(FixedClock::new(t0()));
    (IolClient::new(server.uri(), creds(), clock.clone()), clock)
}

fn envelope(data: Value) -> Value {
    json!({"meta": {"method": "", "path": "", "status_code": 200}, "data": data, "error": null})
}

fn token_body(access: &str, refresh: &str, expires: chrono::DateTime<Utc>) -> Value {
    envelope(json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": 900,
        "expires_at": expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "token_type": "bearer"
    }))
}

fn quote_body() -> Value {
    envelope(json!({
        "symbol": "SPY", "market": "BCBA", "settlement_term": "T1", "currency": "ARS",
        "prices": {"current": 100.5, "previous_close": 99.0},
        "lot_size": 1, "last_updated": "2026-11-23T15:00:00Z", "unknown_field": {"a": 1}
    }))
}

async fn mount_login(server: &MockServer, access: &str, refresh: &str, expect: u64) {
    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .and(body_partial_json(json!({"grant_type": "password"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_body(
            access,
            refresh,
            t0() + Duration::minutes(15),
        )))
        .expect(expect)
        .mount(server)
        .await;
}

#[tokio::test]
async fn login_once_and_reuse_token() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("GET"))
        .and(path("/broker/market-data/SPY/quote"))
        .and(query_param("market", "BCBA"))
        .and(query_param("term", "T1"))
        .and(header("authorization", "Bearer t1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(quote_body()))
        .expect(2)
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let q = c.quote("SPY", Market::Bcba, Term::T1).await.unwrap();
    assert_eq!(q.prices.current, Some(100.5));
    c.quote("SPY", Market::Bcba, Term::T1).await.unwrap();
}

#[tokio::test]
async fn refreshes_when_expired() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .and(body_json(
            json!({"grant_type": "refresh_token", "refresh_token": "r1"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_body(
            "t2",
            "r2",
            t0() + Duration::minutes(40),
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/broker/market-data/SPY/quote"))
        .and(header("authorization", "Bearer t1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(quote_body()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/broker/market-data/SPY/quote"))
        .and(header("authorization", "Bearer t2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(quote_body()))
        .expect(1)
        .mount(&server)
        .await;
    let (c, clock) = client(&server);
    c.quote("SPY", Market::Bcba, Term::T1).await.unwrap();
    clock.advance(Duration::minutes(20));
    c.quote("SPY", Market::Bcba, Term::T1).await.unwrap();
}

#[tokio::test]
async fn relogin_when_refresh_fails() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 2).await;
    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .and(body_partial_json(json!({"grant_type": "refresh_token"})))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "meta": {}, "data": null, "error": {"message": "Token expirado", "code": "E401"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/broker/market-data/SPY/quote"))
        .respond_with(ResponseTemplate::new(200).set_body_json(quote_body()))
        .mount(&server)
        .await;
    let (c, clock) = client(&server);
    c.quote("SPY", Market::Bcba, Term::T1).await.unwrap();
    clock.advance(Duration::minutes(20));
    c.quote("SPY", Market::Bcba, Term::T1).await.unwrap();
}

#[tokio::test]
async fn retries_once_on_401() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 2).await;
    Mock::given(method("GET"))
        .and(path("/broker/market-data/SPY/quote"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "meta": {}, "data": null, "error": {"message": "Token expirado", "code": "E401"}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/broker/market-data/SPY/quote"))
        .respond_with(ResponseTemplate::new(200).set_body_json(quote_body()))
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    c.quote("SPY", Market::Bcba, Term::T1).await.unwrap();
}

#[tokio::test]
async fn second_401_is_an_error() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 2).await;
    Mock::given(method("GET"))
        .and(path("/broker/market-data/SPY/quote"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "meta": {}, "data": null, "error": {"message": "Token expirado", "code": "E401"}
        })))
        .expect(2)
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let err = c.quote("SPY", Market::Bcba, Term::T1).await.unwrap_err();
    assert!(matches!(err, IolError::Api { status: 401, .. }), "{err:?}");
}

#[tokio::test]
async fn failed_login_is_auth_error_without_password() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "meta": {}, "data": null, "error": {"message": "Credenciales inválidas", "code": "E401"}
        })))
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let err = c.quote("SPY", Market::Bcba, Term::T1).await.unwrap_err();
    assert!(matches!(err, IolError::Auth(_)), "{err:?}");
    assert!(!format!("{err} {err:?}").contains(PASSWORD));
}

#[tokio::test]
async fn api_error_envelope_maps() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("GET"))
        .and(path("/broker/accounts/AR/balances"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "meta": {"status_code": 400}, "data": null,
            "error": {"message": "Saldo insuficiente", "code": "E1"}
        })))
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let err = c.balances(Country::Ar).await.unwrap_err();
    match err {
        IolError::Api {
            status,
            code,
            message,
        } => {
            assert_eq!(status, 400);
            assert_eq!(code.as_deref(), Some("E1"));
            assert_eq!(message, "Saldo insuficiente");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn non_envelope_error_uses_status_text() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("GET"))
        .and(path("/broker/accounts/AR/balances"))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"))
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let err = c.balances(Country::Ar).await.unwrap_err();
    assert!(matches!(err, IolError::Api { status: 502, .. }), "{err:?}");
}

#[tokio::test]
async fn order_request_shape() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    let key = Uuid::new_v4();
    Mock::given(method("POST"))
        .and(path("/broker/orders"))
        .and(header("x-idempotency-key", key.to_string().as_str()))
        .and(header("authorization", "Bearer t1"))
        .and(body_json(json!({
            "symbol": "SPY", "market_id": "BCBA", "side": "buy", "order_type": "limit",
            "due_date": "2026-11-24", "limit_price": 1234.5, "quantity": 10.0,
            "settlement_term": "T1"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(envelope(json!({
            "order_id": "98765", "successful_purchase": true,
            "messages": [{"code": "OK", "message": "Orden recibida"}]
        }))))
        .expect(1)
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let req = IolOrderRequest {
        symbol: "SPY".into(),
        market_id: Market::Bcba,
        side: Side::Buy,
        order_type: OrderType::Limit,
        due_date: NaiveDate::from_ymd_opt(2026, 11, 24).unwrap(),
        limit_price: Some(1234.5),
        quantity: 10.0,
        settlement_term: Some(Term::T1),
    };
    let ack = c.place_order(&req, key).await.unwrap();
    assert_eq!(ack.order_id, "98765");
    assert!(ack.successful_purchase);
    assert_eq!(ack.messages.len(), 1);
    // Wire shape checked as a Value too (no notional, no nulls).
    let v = serde_json::to_value(&req).unwrap();
    assert!(v.get("notional").is_none());
}

#[tokio::test]
async fn order_request_omits_optional_fields() {
    let req = IolOrderRequest {
        symbol: "SPY".into(),
        market_id: Market::Nyse,
        side: Side::Sell,
        order_type: OrderType::Market,
        due_date: NaiveDate::from_ymd_opt(2026, 11, 24).unwrap(),
        limit_price: None,
        quantity: 3.0,
        settlement_term: None,
    };
    assert_eq!(
        serde_json::to_value(&req).unwrap(),
        json!({"symbol": "SPY", "market_id": "NYSE", "side": "sell", "order_type": "market",
               "due_date": "2026-11-24", "quantity": 3.0})
    );
}

#[tokio::test]
async fn estimate_and_cancel() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("POST"))
        .and(path("/broker/orders/estimate"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "amount": {"currency": "ARS", "value": 1000.0},
            "commission": {"currency": "ARS", "value": 5.5},
            "regulatory_fees": {"market_fee": {"currency": "ARS", "value": 1.0}, "vat": null}
        }))))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/broker/orders/98765"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "order_id": "98765", "status": "cancelled", "cancelled": true
        }))))
        .expect(1)
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let req = IolOrderRequest {
        symbol: "SPY".into(),
        market_id: Market::Bcba,
        side: Side::Buy,
        order_type: OrderType::Market,
        due_date: NaiveDate::from_ymd_opt(2026, 11, 24).unwrap(),
        limit_price: None,
        quantity: 1.0,
        settlement_term: None,
    };
    let est = c.estimate_order(&req).await.unwrap();
    assert_eq!(est.commission.unwrap().value, 5.5);
    let cancel = c.cancel_order("98765").await.unwrap();
    assert!(cancel.cancelled);
}

#[test]
fn bar_date_is_argentina_local() {
    let mk = |ts| IolBar {
        open: 1.0,
        high: 2.0,
        low: 0.5,
        close: 1.5,
        volume: 10.0,
        timestamp: ts,
    };
    let b = mk(1732417200).to_core_bar();
    assert_eq!(b.date, NaiveDate::from_ymd_opt(2024, 11, 24).unwrap());
    assert_eq!(b.adj_close, b.close);
    let b = mk(1732402800).to_core_bar();
    assert_eq!(b.date, NaiveDate::from_ymd_opt(2024, 11, 23).unwrap());
}

#[tokio::test]
async fn bars_query_and_parse() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("GET"))
        .and(path("/broker/market-data/SPY/bars"))
        .and(query_param("market", "NYSE"))
        .and(query_param("from", "2024-01-01 00:00:00"))
        .and(query_param("to", "2024-01-31 23:59:59"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "bars": [{"open": 1.0, "high": 2.0, "low": 0.5, "close": 1.5, "timestamp": 1732417200,
                      "datetime": "x"}],
            "symbol": "SPY", "market": "NYSE", "total_bars": 1
        }))))
        .expect(1)
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let bars = c
        .bars(
            "SPY",
            Market::Nyse,
            NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].volume, 0.0);
}

#[tokio::test]
async fn positions_tolerate_missing_fields() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("GET"))
        .and(path("/broker/accounts/AR/positions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "positions": [
                {"quantity": 5, "instrument": {"symbol": "SPY"}},
                {"quantity": 2.5, "instrument": {"symbol": "QQQ", "type": "CEDEAR", "lot_size": 1},
                 "availability": {"quantity_available": 2.5}, "market_value": {"current_price": 10.0},
                 "extra": 1}
            ]
        }))))
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let p = c.positions(Country::Ar).await.unwrap();
    assert_eq!(p.positions.len(), 2);
    assert!(p.positions[0].availability.is_none());
    assert!(p.total_market_value.is_none());
    assert_eq!(
        p.positions[1].instrument.instrument_type.as_deref(),
        Some("CEDEAR")
    );
    assert_eq!(
        p.positions[1].market_value.as_ref().unwrap().current_price,
        Some(10.0)
    );
}

#[tokio::test]
async fn balances_term_lookup() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("GET"))
        .and(path("/broker/accounts/AR/balances"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "ars": {"currency": "ARS", "totals": {"t0": 1, "t1": 2},
                    "available": {"t0": 10.0, "t1": 20.5, "t2": 30.0, "t3": 40.0},
                    "available_for_withdrawal": 7.0}
        }))))
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let b = c.balances(Country::Ar).await.unwrap();
    let ars = b.ars.unwrap();
    assert_eq!(ars.available.get(Term::T1), Some(20.5));
    assert_eq!(ars.available.get(Term::T0), Some(10.0));
    assert_eq!(ars.committed.get(Term::T1), None);
    assert!(b.usd.is_none());
}

#[tokio::test]
async fn operations_parse_status_and_executed() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("GET"))
        .and(path("/broker/accounts/activities/operations"))
        .and(query_param("start_date", "2026-11-01"))
        .and(query_param("end_date", "2026-11-23"))
        .and(query_param("country", "AR"))
        .and(query_param("state", "all"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
            {"order_id": "1", "status": "completed", "symbol": "SPY",
             "order": {"qty": 10, "price": 100.0, "amount": 1000.0},
             "executed": {"qty": 10, "price": 99.5, "amount": 995.0}},
            {"order_id": "2", "status": "pending"}
        ]))))
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let ops = c
        .operations(&OperationsQuery {
            start_date: NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            end_date: NaiveDate::from_ymd_opt(2026, 11, 23).unwrap(),
            country: Some("AR".into()),
            symbol: None,
            state: Some("all".into()),
        })
        .await
        .unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0].status, "completed");
    assert_eq!(ops[0].executed.as_ref().unwrap().qty, Some(10.0));
    assert!(ops[1].executed.is_none());
}

#[tokio::test]
async fn activities_parse() {
    let server = MockServer::start().await;
    mount_login(&server, "t1", "r1", 1).await;
    Mock::given(method("GET"))
        .and(path("/broker/accounts/activities"))
        .and(query_param("start_date", "2026-11-01"))
        .and(query_param("end_date", "2026-11-23"))
        .and(query_param("movement_type", "deposit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
            {"id": "a1", "activity_type": "deposit", "net_amount": 500.0, "currency": "ARS",
             "fees": {"commission": 1.0}}
        ]))))
        .mount(&server)
        .await;
    let (c, _) = client(&server);
    let acts = c
        .activities(
            NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 11, 23).unwrap(),
            Some("deposit"),
        )
        .await
        .unwrap();
    assert_eq!(acts[0].net_amount, Some(500.0));
    assert_eq!(acts[0].fees.as_ref().unwrap().commission, Some(1.0));
}

#[test]
fn credentials_debug_hides_password() {
    let s = format!("{:?}", creds());
    assert!(!s.contains(PASSWORD));
    assert!(s.contains("***"));
}

#[tokio::test]
async fn client_debug_hides_secrets() {
    let server = MockServer::start().await;
    let (c, _) = client(&server);
    assert!(!format!("{c:?}").contains(PASSWORD));
}
