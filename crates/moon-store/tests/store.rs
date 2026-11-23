use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};
use moon_core::asset::{AssetId, Currency, Market};
use moon_core::bars::Bar;
use moon_core::testutil::d;
use moon_core::types::{DataSource, ExecMode, OrderReason, OrderStatus, Side};
use moon_store::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use uuid::Uuid;

fn ts(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2025, 1, day, 12, 0, 0).unwrap()
}

fn bar(y: i32, m: u32, day: u32, close: f64) -> Bar {
    Bar {
        date: d(y, m, day),
        open: close,
        high: close + 1.0,
        low: close - 1.0,
        close,
        adj_close: close * 0.99,
        volume: 1000.0,
    }
}

fn order(status: OrderStatus, day: u32) -> OrderRecord {
    OrderRecord {
        id: Uuid::new_v4(),
        batch_id: Uuid::new_v4(),
        created_at: ts(day),
        updated_at: ts(day),
        mode: ExecMode::Paper,
        reason: OrderReason::Monthly,
        asset: AssetId::new("SPY"),
        symbol: "SPY".into(),
        market: Market::Bcba,
        side: Side::Buy,
        qty: dec!(10),
        limit_price: Some(dec!(1234.5678)),
        currency: Currency::Ars,
        status,
        broker_order_id: None,
        filled_qty: Decimal::ZERO,
        avg_fill_price: None,
        fees: Decimal::ZERO,
        fees_estimated: true,
        error: None,
    }
}

fn snapshot(cash: Decimal, qty: Decimal) -> Snapshot {
    let mut positions = BTreeMap::new();
    positions.insert("SPY".to_string(), qty);
    Snapshot {
        at: ts(1),
        mode: ExecMode::Paper,
        cash_ars: cash,
        equity_ars: dec!(1000.55),
        ccl: dec!(1200.5),
        equity_usd: dec!(0.83),
        nav: 100.25,
        units: dec!(10),
        drawdown: -0.05,
        positions,
    }
}

#[test]
fn migrations_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    assert_eq!(Store::open(&path).unwrap().schema_version().unwrap(), 1);
    assert_eq!(Store::open(&path).unwrap().schema_version().unwrap(), 1);
}

#[test]
fn bars_upsert_replaces_same_date() {
    let mut s = Store::open_in_memory().unwrap();
    s.upsert_bars("SPY", DataSource::Yahoo, &[bar(2025, 1, 2, 10.0)])
        .unwrap();
    let n = s
        .upsert_bars("SPY", DataSource::Yahoo, &[bar(2025, 1, 2, 20.0)])
        .unwrap();
    assert_eq!(n, 1);
    let bars = s.load_bars("SPY", DataSource::Yahoo).unwrap();
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].close, 20.0);
    assert!(s.load_bars("SPY", DataSource::Iol).unwrap().is_empty());
}

#[test]
fn bars_load_sorted() {
    let mut s = Store::open_in_memory().unwrap();
    s.upsert_bars(
        "SPY",
        DataSource::Yahoo,
        &[
            bar(2025, 1, 5, 3.0),
            bar(2025, 1, 2, 1.0),
            bar(2025, 1, 3, 2.0),
        ],
    )
    .unwrap();
    let bars = s.load_bars("SPY", DataSource::Yahoo).unwrap();
    let dates: Vec<_> = bars.iter().map(|b| b.date).collect();
    assert_eq!(dates, vec![d(2025, 1, 2), d(2025, 1, 3), d(2025, 1, 5)]);
    assert_eq!(bars[0], bar(2025, 1, 2, 1.0));
}

#[test]
fn last_bar_date() {
    let mut s = Store::open_in_memory().unwrap();
    assert_eq!(s.last_bar_date("SPY", DataSource::Yahoo).unwrap(), None);
    s.upsert_bars(
        "SPY",
        DataSource::Yahoo,
        &[bar(2025, 1, 2, 1.0), bar(2025, 1, 9, 2.0)],
    )
    .unwrap();
    assert_eq!(
        s.last_bar_date("SPY", DataSource::Yahoo).unwrap(),
        Some(d(2025, 1, 9))
    );
}

#[test]
fn decimal_round_trip_exact() {
    let mut s = Store::open_in_memory().unwrap();
    let snap = snapshot(dec!(0.1) + dec!(0.2), dec!(123456789.123456789));
    s.insert_snapshot(&snap).unwrap();
    let back = s.latest_snapshot(ExecMode::Paper).unwrap().unwrap();
    assert_eq!(back.cash_ars, dec!(0.3));
    assert_eq!(back.positions["SPY"], dec!(123456789.123456789));
    assert_eq!(back, snap);
    assert!(s.latest_snapshot(ExecMode::Live).unwrap().is_none());
    let between = s.snapshots_between(ExecMode::Paper, ts(1), ts(2)).unwrap();
    assert_eq!(between.len(), 1);
}

#[test]
fn order_insert_is_idempotent() {
    let mut s = Store::open_in_memory().unwrap();
    let o = order(OrderStatus::PendingSend, 1);
    assert!(s.insert_order(&o).unwrap());
    assert!(!s.insert_order(&o).unwrap());
    assert_eq!(s.get_order(o.id).unwrap().unwrap(), o);
    assert!(s.get_order(Uuid::new_v4()).unwrap().is_none());
}

#[test]
fn open_orders_excludes_terminal() {
    let mut s = Store::open_in_memory().unwrap();
    let a = order(OrderStatus::Sent, 1);
    let b = order(OrderStatus::Filled, 2);
    let c = order(OrderStatus::PartiallyFilled, 3);
    let e = order(OrderStatus::Cancelled, 4);
    for o in [&a, &b, &c, &e] {
        s.insert_order(o).unwrap();
    }
    let open = s.open_orders(ExecMode::Paper).unwrap();
    let ids: Vec<_> = open.iter().map(|o| o.id).collect();
    assert_eq!(ids, vec![a.id, c.id]);
    assert!(s.open_orders(ExecMode::Live).unwrap().is_empty());
    let between = s.orders_between(ExecMode::Paper, ts(2), ts(3)).unwrap();
    assert_eq!(between.len(), 2);
}

#[test]
fn update_order_persists_fill() {
    let mut s = Store::open_in_memory().unwrap();
    let mut o = order(OrderStatus::Sent, 1);
    s.insert_order(&o).unwrap();
    o.status = OrderStatus::Filled;
    o.filled_qty = dec!(10);
    o.avg_fill_price = Some(dec!(1230.1234));
    o.fees = dec!(12.34);
    o.fees_estimated = false;
    o.broker_order_id = Some("B-1".into());
    o.updated_at = ts(2);
    s.update_order(&o).unwrap();
    assert_eq!(s.get_order(o.id).unwrap().unwrap(), o);
}

fn proposal() -> Proposal {
    Proposal {
        id: Uuid::new_v4(),
        created_at: ts(1),
        expires_at: ts(2),
        kind: ProposalKind::Monthly,
        status: ProposalStatus::Pending,
        decided_at: None,
        payload: serde_json::json!({"a": [1, 2]}),
        telegram_message_id: None,
    }
}

#[test]
fn proposal_lifecycle() {
    let mut s = Store::open_in_memory().unwrap();
    let p = proposal();
    s.insert_proposal(&p).unwrap();
    assert_eq!(s.get_proposal(p.id).unwrap().unwrap(), p);
    assert_eq!(s.pending_proposals().unwrap().len(), 1);
    s.set_proposal_message(p.id, 42).unwrap();
    s.set_proposal_status(p.id, ProposalStatus::Approved, ts(1))
        .unwrap();
    assert!(s.pending_proposals().unwrap().is_empty());
    s.set_proposal_status(p.id, ProposalStatus::Executed, ts(2))
        .unwrap();
    let back = s.get_proposal(p.id).unwrap().unwrap();
    assert_eq!(back.status, ProposalStatus::Executed);
    assert_eq!(back.decided_at, Some(ts(2)));
    assert_eq!(back.telegram_message_id, Some(42));
    assert!(s.pending_proposals().unwrap().is_empty());
}

#[test]
fn flows_dedupe_by_external_id() {
    let mut s = Store::open_in_memory().unwrap();
    let f = FlowRecord {
        at: ts(3),
        mode: ExecMode::Live,
        kind: FlowKind::Deposit,
        amount_ars: dec!(1000.10),
        external_id: Some("X1".into()),
        note: "dep".into(),
    };
    assert!(s.insert_flow(&f).unwrap());
    assert!(!s.insert_flow(&f).unwrap());
    let none = FlowRecord {
        external_id: None,
        kind: FlowKind::Withdrawal,
        ..f.clone()
    };
    assert!(s.insert_flow(&none).unwrap());
    assert!(s.insert_flow(&none).unwrap());
    let got = s.flows_between(ExecMode::Live, ts(1), ts(5)).unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], f);
    assert!(
        s.flows_between(ExecMode::Paper, ts(1), ts(5))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn state_generic_round_trip() {
    let mut s = Store::open_in_memory().unwrap();
    let mut m: BTreeMap<String, i64> = BTreeMap::new();
    m.insert("a".into(), 1);
    m.insert("b".into(), -2);
    s.set_state("map", &m).unwrap();
    s.set_state("flag", &true).unwrap();
    assert_eq!(
        s.get_state::<BTreeMap<String, i64>>("map").unwrap(),
        Some(m)
    );
    assert_eq!(s.get_state::<bool>("flag").unwrap(), Some(true));
    s.set_state("flag", &false).unwrap();
    assert_eq!(s.get_state::<bool>("flag").unwrap(), Some(false));
    s.delete_state("flag").unwrap();
    assert_eq!(s.get_state::<bool>("flag").unwrap(), None);
    assert_eq!(s.get_state::<bool>("missing").unwrap(), None);
}

#[test]
fn paper_account_default_then_saved() {
    let mut s = Store::open_in_memory().unwrap();
    assert_eq!(s.paper_account().unwrap(), PaperAccount::default());
    let mut a = PaperAccount {
        cash_ars: dec!(1000.01),
        ..Default::default()
    };
    a.positions.insert("SPY".into(), dec!(3.5));
    s.save_paper_account(&a).unwrap();
    assert_eq!(s.paper_account().unwrap(), a);
    a.cash_ars = dec!(5);
    s.save_paper_account(&a).unwrap();
    assert_eq!(s.paper_account().unwrap(), a);
}

#[test]
fn fx_and_decisions() {
    let mut s = Store::open_in_memory().unwrap();
    assert!(s.latest_fx().unwrap().is_none());
    let a = FxRecord {
        at: ts(1),
        rate: dec!(1100.5),
        method: "ccl".into(),
        components: serde_json::json!({"x": 1}),
    };
    let b = FxRecord {
        at: ts(2),
        rate: dec!(1200.25),
        ..a.clone()
    };
    s.insert_fx(&b).unwrap();
    s.insert_fx(&a).unwrap();
    assert_eq!(s.latest_fx().unwrap().unwrap(), b);

    let id1 = s.log_decision(ts(1), "k", &serde_json::json!(1)).unwrap();
    let id2 = s.log_decision(ts(2), "k", &serde_json::json!(2)).unwrap();
    assert!(id2 > id1);
    let rec = s.recent_decisions(1).unwrap();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].id, id2);
    assert_eq!(s.recent_decisions(10).unwrap().len(), 2);
}

#[test]
fn backtest_registry_counts_distinct_hashes() {
    let mut s = Store::open_in_memory().unwrap();
    for (i, h) in ["h1", "h2", "h1"].iter().enumerate() {
        let run = BacktestRun {
            id: None,
            created_at: ts(1 + i as u32),
            config_hash: (*h).into(),
            config: serde_json::json!({"i": i}),
            start: d(2010, 1, 1),
            end: d(2020, 1, 1),
            include_holdout: i == 2,
            metrics: serde_json::json!({"cagr": 0.1}),
        };
        s.register_backtest(&run).unwrap();
    }
    assert_eq!(s.count_distinct_backtest_configs().unwrap(), 2);
    let runs = s.backtest_runs().unwrap();
    assert_eq!(runs.len(), 3);
    assert!(runs[0].include_holdout);
    assert!(runs[0].id.is_some());
}

#[test]
fn corrupt_value_is_reported() {
    let mut s = Store::open_in_memory().unwrap();
    s.set_state("k", &1).unwrap();
    assert!(matches!(
        s.get_state::<String>("k"),
        Err(StoreError::Corrupt(_))
    ));
}

#[test]
fn store_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Store>();
}

#[test]
fn malformed_rows_are_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.db");
    let mut s = Store::open(&path).unwrap();
    let o = order(OrderStatus::Sent, 1);
    s.insert_order(&o).unwrap();
    s.insert_snapshot(&snapshot(dec!(1), dec!(1))).unwrap();
    drop(s);
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute("UPDATE orders SET qty = 'abc'", []).unwrap();
    raw.execute("UPDATE snapshots SET cash_ars = 'zzz'", [])
        .unwrap();
    drop(raw);
    let s = Store::open(&path).unwrap();
    assert!(matches!(s.get_order(o.id), Err(StoreError::Corrupt(_))));
    assert!(matches!(
        s.latest_snapshot(ExecMode::Paper),
        Err(StoreError::Corrupt(_))
    ));
}

#[test]
fn updates_on_missing_ids_are_not_found() {
    let mut s = Store::open_in_memory().unwrap();
    let o = order(OrderStatus::Sent, 1);
    assert!(matches!(s.update_order(&o), Err(StoreError::NotFound(_))));
    let id = Uuid::new_v4();
    assert!(matches!(
        s.set_proposal_status(id, ProposalStatus::Approved, ts(1)),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        s.set_proposal_message(id, 1),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn newer_schema_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("n.db");
    drop(Store::open(&path).unwrap());
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch("PRAGMA user_version = 99").unwrap();
    drop(raw);
    assert!(matches!(Store::open(&path), Err(StoreError::Corrupt(_))));
}

#[test]
fn apply_paper_fill_persists_account_and_state() {
    let mut s = Store::open_in_memory().unwrap();
    let mut a = PaperAccount::default();
    a.cash_ars = rust_decimal_macros::dec!(123.45);
    a.positions
        .insert("SPY".into(), rust_decimal_macros::dec!(2));
    s.apply_paper_fill(&a, "paper_order:k", &serde_json::json!({"x": 1}))
        .unwrap();
    assert_eq!(s.paper_account().unwrap(), a);
    let v: Option<serde_json::Value> = s.get_state("paper_order:k").unwrap();
    assert_eq!(v, Some(serde_json::json!({"x": 1})));
}
