#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use moon_broker::*;
use moon_core::asset::{Currency, Market};
use moon_core::fees::FeeModel;
use moon_core::time::FixedClock;
use moon_core::types::{ExecMode, OrderStatus, OrderType, Side, Term};
use moon_iol::{IolClient, IolCredentials};
use moon_store::{FlowKind, PaperAccount, Store};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 23, 15, 0, 0).unwrap()
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

// ---------- paper ----------

fn fees() -> FeeModel {
    FeeModel {
        commission_rate: dec!(0.006),
        market_fee_rate: dec!(0.0008),
        vat_rate: dec!(0.21),
        min_commission: Decimal::ZERO,
    }
}

fn paper(
    cash: Decimal,
    positions: &[(&str, Decimal)],
) -> (PaperBroker, Arc<StaticQuotes>, Arc<Mutex<Store>>) {
    let mut store = Store::open_in_memory().unwrap();
    store
        .save_paper_account(&PaperAccount {
            cash_ars: cash,
            positions: positions
                .iter()
                .map(|(s, q)| (s.to_string(), *q))
                .collect::<BTreeMap<_, _>>(),
        })
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let quotes = Arc::new(StaticQuotes::new());
    quotes.set("SPY", Market::Bcba, dec!(10000), Currency::Ars);
    let broker = PaperBroker::new(
        store.clone(),
        quotes.clone(),
        fees(),
        dec!(0.005),
        Arc::new(FixedClock::new(t0())),
    );
    (broker, quotes, store)
}

fn ticket(side: Side, qty: Decimal, limit: Option<Decimal>) -> OrderTicket {
    OrderTicket {
        idempotency_key: Uuid::new_v4(),
        symbol: "SPY".into(),
        market: Market::Bcba,
        side,
        qty,
        order_type: if limit.is_some() {
            OrderType::Limit
        } else {
            OrderType::Market
        },
        limit_price: limit,
        due_date: d(2026, 11, 24),
        settlement_term: Term::T1,
    }
}

#[tokio::test]
async fn paper_buy_applies_slippage_and_fees() {
    let (b, _, _) = paper(dec!(1000000), &[]);
    assert_eq!(b.mode(), ExecMode::Paper);
    let r = b.place(&ticket(Side::Buy, dec!(10), None)).await.unwrap();
    assert!(r.accepted);
    let f = r.fill.unwrap();
    assert_eq!(f.status, OrderStatus::Filled);
    assert_eq!(f.filled_qty, dec!(10));
    assert_eq!(f.avg_price, Some(dec!(10050.0000)));
    assert_eq!(f.fees, Some(dec!(810.03)));
    let a = b.account().await.unwrap();
    assert_eq!(a.cash_ars, dec!(898689.97));
    assert_eq!(a.positions.get("SPY"), Some(&dec!(10)));
    assert_eq!(a.sellable, a.positions);
}

#[tokio::test]
async fn paper_rejects_insufficient_cash_and_keeps_state() {
    let (b, _, _) = paper(dec!(50000), &[]);
    let r = b.place(&ticket(Side::Buy, dec!(10), None)).await.unwrap();
    assert!(!r.accepted);
    assert_eq!(
        r.messages,
        vec!["efectivo insuficiente (paper)".to_string()]
    );
    assert!(r.fill.is_none());
    let a = b.account().await.unwrap();
    assert_eq!(a.cash_ars, dec!(50000));
    assert!(a.positions.is_empty());
}

#[tokio::test]
async fn paper_rejects_selling_more_than_held() {
    let (b, _, _) = paper(dec!(0), &[("SPY", dec!(5))]);
    let r = b.place(&ticket(Side::Sell, dec!(6), None)).await.unwrap();
    assert!(!r.accepted);
    assert_eq!(
        r.messages,
        vec!["tenencia insuficiente (paper)".to_string()]
    );
    assert_eq!(b.account().await.unwrap().positions["SPY"], dec!(5));
}

#[tokio::test]
async fn paper_sell_credits_cash_and_removes_empty_position() {
    let (b, _, _) = paper(dec!(0), &[("SPY", dec!(5))]);
    let r = b.place(&ticket(Side::Sell, dec!(5), None)).await.unwrap();
    assert!(r.accepted);
    let a = b.account().await.unwrap();
    // fill 9950, value 49750, fees 298.50 + 39.80 + 62.69 = 400.99
    assert_eq!(a.cash_ars, dec!(49349.01));
    assert!(a.positions.is_empty());
}

#[tokio::test]
async fn paper_limit_not_marketable() {
    let (b, _, _) = paper(dec!(1000000), &[("SPY", dec!(5))]);
    let r = b
        .place(&ticket(Side::Buy, dec!(1), Some(dec!(10000))))
        .await
        .unwrap();
    assert!(!r.accepted);
    assert_eq!(
        r.messages,
        vec!["precio límite no alcanzado (paper)".to_string()]
    );
    let r = b
        .place(&ticket(Side::Sell, dec!(1), Some(dec!(10000))))
        .await
        .unwrap();
    assert!(!r.accepted);
    let r = b
        .place(&ticket(Side::Buy, dec!(1), Some(dec!(10050))))
        .await
        .unwrap();
    assert!(r.accepted);
}

#[tokio::test]
async fn paper_place_is_idempotent() {
    let (b, _, _) = paper(dec!(1000000), &[]);
    let t = ticket(Side::Buy, dec!(10), None);
    let r1 = b.place(&t).await.unwrap();
    let r2 = b.place(&t).await.unwrap();
    assert_eq!(r1, r2);
    assert_eq!(r1.broker_order_id, format!("paper-{}", t.idempotency_key));
    assert_eq!(b.account().await.unwrap().cash_ars, dec!(898689.97));
    let p = b
        .refresh(&r1.broker_order_id, d(2026, 11, 23))
        .await
        .unwrap();
    assert_eq!(p.status, OrderStatus::Filled);
    assert!(matches!(
        b.refresh("paper-nope", d(2026, 11, 23)).await,
        Err(BrokerError::NotFound(_))
    ));
    assert!(b.cancel("x").await.is_ok());
    assert!(
        b.external_flows(d(2026, 11, 1), d(2026, 11, 23))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn paper_rejects_usd_quote_for_bcba() {
    let (b, quotes, _) = paper(dec!(1000000), &[]);
    quotes.set("SPY", Market::Bcba, dec!(10), Currency::Usd);
    let err = b
        .place(&ticket(Side::Buy, dec!(1), None))
        .await
        .unwrap_err();
    assert!(
        matches!(err, BrokerError::CurrencyMismatch { .. }),
        "{err:?}"
    );
}

// ---------- IOL ----------

fn envelope(data: Value) -> Value {
    json!({"meta": {"status_code": 200}, "data": data, "error": null})
}

async fn iol_server() -> (MockServer, IolBroker) {
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
    let clock = Arc::new(FixedClock::new(t0()));
    let client = Arc::new(IolClient::new(
        server.uri(),
        IolCredentials {
            username: "user".into(),
            password: "dummy-test-pw".into(),
        },
        clock.clone(),
    ));
    (server, IolBroker::new(client, Term::T1, clock))
}

#[tokio::test]
async fn iol_broker_account_maps_balances_and_positions() {
    let (server, b) = iol_server().await;
    assert_eq!(b.mode(), ExecMode::Live);
    Mock::given(method("GET"))
        .and(path("/broker/accounts/AR/balances"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "ars": {"currency": "ARS", "totals": {"t1": 999.0},
                    "available": {"t0": 1.0, "t1": 12345.5}}
        }))))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/broker/accounts/AR/positions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "positions": [
                {"quantity": 10, "instrument": {"symbol": "SPY"},
                 "availability": {"quantity_available": 7}},
                {"quantity": 5, "instrument": {"symbol": "SPY"}},
                {"quantity": 3, "instrument": {"symbol": "QQQ"}}
            ]
        }))))
        .mount(&server)
        .await;
    let a = b.account().await.unwrap();
    assert_eq!(a.cash_ars, dec!(12345.5));
    assert_eq!(a.positions["SPY"], dec!(15));
    assert_eq!(a.positions["QQQ"], dec!(3));
    assert_eq!(a.sellable["SPY"], dec!(12));
    assert_eq!(a.sellable["QQQ"], dec!(3));
    assert_eq!(a.as_of, t0());
}

#[tokio::test]
async fn iol_place_maps_ticket_and_ack() {
    let (server, b) = iol_server().await;
    Mock::given(method("POST"))
        .and(path("/broker/orders"))
        .and(body_partial_json(json!({
            "symbol": "SPY", "market_id": "BCBA", "side": "buy", "quantity": 10.0,
            "limit_price": 1234.5, "settlement_term": "T1"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(envelope(json!({
            "order_id": "98765", "successful_purchase": true,
            "messages": [{"code": "OK", "message": "Orden recibida"}]
        }))))
        .mount(&server)
        .await;
    let r = b
        .place(&ticket(Side::Buy, dec!(10), Some(dec!(1234.5))))
        .await
        .unwrap();
    assert_eq!(r.broker_order_id, "98765");
    assert!(r.accepted);
    assert_eq!(r.messages, vec!["Orden recibida".to_string()]);
    assert!(r.fill.is_none());
}

#[tokio::test]
async fn iol_refresh_status_mapping() {
    let (server, b) = iol_server().await;
    Mock::given(method("GET"))
        .and(path("/broker/accounts/activities/operations"))
        .and(query_param("start_date", "2026-11-20"))
        .and(query_param("end_date", "2026-11-23"))
        .and(query_param("country", "AR"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
            {"order_id": "c", "status": "completed",
             "executed": {"qty": 10, "price": 99.5}},
            {"order_id": "p1", "status": "pending", "executed": {"qty": 4, "price": 100.0}},
            {"order_id": "p0", "status": "pending", "executed": {"qty": 0, "price": 0}},
            {"order_id": "x", "status": "cancelled"},
            {"order_id": "r", "status": "rejected"},
            {"order_id": "w", "status": "weird"}
        ]))))
        .mount(&server)
        .await;
    let from = d(2026, 11, 20);
    let get = |id: &'static str| {
        let b = &b;
        async move { b.refresh(id, from).await.unwrap() }
    };
    let c = get("c").await;
    assert_eq!(c.status, OrderStatus::Filled);
    assert_eq!(c.filled_qty, dec!(10));
    assert_eq!(c.avg_price, Some(dec!(99.5)));
    assert_eq!(c.fees, None);
    let p1 = get("p1").await;
    assert_eq!(p1.status, OrderStatus::PartiallyFilled);
    assert_eq!(p1.filled_qty, dec!(4));
    let p0 = get("p0").await;
    assert_eq!(p0.status, OrderStatus::Sent);
    assert_eq!(p0.avg_price, None);
    assert_eq!(get("x").await.status, OrderStatus::Cancelled);
    assert_eq!(get("r").await.status, OrderStatus::Rejected);
    assert_eq!(get("w").await.status, OrderStatus::Sent);
    assert_eq!(get("missing").await.status, OrderStatus::Sent);
}

#[tokio::test]
async fn iol_cancel_calls_delete() {
    let (server, b) = iol_server().await;
    Mock::given(method("DELETE"))
        .and(path("/broker/orders/77"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!({
            "order_id": "77", "status": "cancelled", "cancelled": true
        }))))
        .expect(1)
        .mount(&server)
        .await;
    b.cancel("77").await.unwrap();
}

#[tokio::test]
async fn iol_external_flows_filters_deposits() {
    let (server, b) = iol_server().await;
    Mock::given(method("GET"))
        .and(path("/broker/accounts/activities"))
        .respond_with(ResponseTemplate::new(200).set_body_json(envelope(json!([
            {"id": "d1", "activity_type": "Depósito", "net_amount": 500.0,
             "currency": "ARS", "status": "completed", "trade_date": "2026-11-20T10:00:00Z"},
            {"id": "w1", "activity_type": "extraction", "net_amount": -200.0,
             "currency": "ARS", "status": "completed"},
            {"id": "d2", "activity_type": "deposit", "net_amount": 100.0,
             "currency": "ARS", "status": "pending"},
            {"id": "d3", "activity_type": "deposit", "net_amount": 100.0,
             "currency": "USD", "status": "completed"},
            {"id": "t1", "activity_type": "trade", "net_amount": 100.0,
             "currency": "ARS", "status": "completed"}
        ]))))
        .mount(&server)
        .await;
    let flows = b
        .external_flows(d(2026, 11, 1), d(2026, 11, 23))
        .await
        .unwrap();
    assert_eq!(flows.len(), 2);
    assert_eq!(flows[0].external_id, "d1");
    assert_eq!(flows[0].kind, FlowKind::Deposit);
    assert_eq!(flows[0].amount_ars, dec!(500));
    assert_eq!(
        flows[0].at,
        Utc.with_ymd_and_hms(2026, 11, 20, 10, 0, 0).unwrap()
    );
    assert_eq!(flows[1].kind, FlowKind::Withdrawal);
    assert_eq!(flows[1].amount_ars, dec!(200));
}

// ---------- CCL / currency ----------

fn pair(local: &str, adr: &str) -> FxPair {
    FxPair {
        local_symbol: local.into(),
        adr_symbol: adr.into(),
        adr_market: Market::Nyse,
        ratio: dec!(1),
    }
}

#[tokio::test]
async fn implied_ccl_median_skips_bad_pairs() {
    let q = StaticQuotes::new();
    q.set("AAA", Market::Bcba, dec!(1180), Currency::Ars);
    q.set("AAA", Market::Nyse, dec!(1), Currency::Usd);
    q.set("BBB", Market::Bcba, dec!(2400), Currency::Ars);
    q.set("BBB", Market::Nyse, dec!(2), Currency::Usd);
    q.set("CCC", Market::Bcba, dec!(900), Currency::Ars);
    q.set("CCC", Market::Nyse, dec!(1000), Currency::Ars); // ADR in ARS: skipped
    let pairs = [pair("AAA", "AAA"), pair("BBB", "BBB"), pair("CCC", "CCC")];
    let fx = implied_ccl(&q, &pairs, 2, t0()).await.unwrap();
    assert_eq!(fx.rate, dec!(1190.00));
    assert_eq!(fx.components.len(), 2);
    assert_eq!(fx.at, t0());
    let err = implied_ccl(&q, &pairs, 3, t0()).await.unwrap_err();
    assert!(matches!(err, BrokerError::Quote(_)), "{err:?}");
}

#[test]
fn parse_currency_variants() {
    assert_eq!(parse_currency("ARS"), Some(Currency::Ars));
    assert_eq!(parse_currency("peso_Argentino"), Some(Currency::Ars));
    assert_eq!(parse_currency("usd"), Some(Currency::Usd));
    assert_eq!(parse_currency("Dólar Estadounidense"), Some(Currency::Usd));
    assert_eq!(parse_currency("DOLAR"), Some(Currency::Usd));
    assert_eq!(parse_currency("EUR"), None);
}
