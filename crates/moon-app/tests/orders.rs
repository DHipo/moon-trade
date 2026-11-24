use chrono::{TimeZone, Utc};
use clap::Parser;
use moon_app::cli::{Cli, Command, OrdersCmd};
use moon_app::commands::orders::{ResolveOutcome, find_order_id, list_text, resolve_order};
use moon_app::engine::UNKNOWN_OUTCOME;
use moon_app::engine::short_id;
use moon_core::asset::{AssetId, Currency, Market};
use moon_core::fees::FeeModel;
use moon_core::types::{ExecMode, OrderReason, OrderStatus, Side};
use moon_store::{OrderRecord, Store};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use uuid::Uuid;

fn unknown_order(limit: Option<Decimal>) -> OrderRecord {
    let at = Utc
        .with_ymd_and_hms(2026, 11, 23, 14, 30, 0)
        .single()
        .expect("time");
    OrderRecord {
        id: Uuid::new_v4(),
        batch_id: Uuid::new_v4(),
        created_at: at,
        updated_at: at,
        mode: ExecMode::Live,
        reason: OrderReason::Emergency,
        asset: AssetId::new("AAA"),
        symbol: "AAA".to_string(),
        market: Market::Bcba,
        side: Side::Sell,
        qty: dec!(100),
        limit_price: limit,
        currency: Currency::Ars,
        status: OrderStatus::Failed,
        broker_order_id: None,
        filled_qty: Decimal::ZERO,
        avg_fill_price: None,
        fees: Decimal::ZERO,
        fees_estimated: false,
        error: Some(format!("{UNKNOWN_OUTCOME} timeout de red")),
    }
}

fn store_with(order: &OrderRecord) -> Store {
    let mut store = Store::open_in_memory().expect("store");
    store.insert_order(order).expect("insert");
    store
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 23, 18, 0, 0)
        .single()
        .expect("time")
}

#[test]
fn resolve_filled_marks_the_order_executed_with_estimated_fees() {
    let order = unknown_order(Some(dec!(9700)));
    let mut store = store_with(&order);
    let fees = FeeModel::zero();
    let msg = resolve_order(
        &mut store,
        &fees,
        now(),
        order.id,
        ResolveOutcome::Filled,
        None,
        Some(dec!(9800)),
    )
    .expect("resolve");
    assert!(msg.contains("ejecutada"), "{msg}");
    let o = store.get_order(order.id).expect("get").expect("order");
    assert_eq!(o.status, OrderStatus::Filled);
    assert_eq!(o.filled_qty, dec!(100));
    assert_eq!(o.avg_fill_price, Some(dec!(9800)));
    assert!(o.fees_estimated);
    assert!(
        !o.error
            .as_deref()
            .unwrap_or("")
            .starts_with(UNKNOWN_OUTCOME),
        "no longer unknown: {:?}",
        o.error
    );
    assert_eq!(o.updated_at, now());

    // Resolving twice is refused and changes nothing.
    let err = resolve_order(
        &mut store,
        &fees,
        now(),
        order.id,
        ResolveOutcome::Cancelled,
        None,
        None,
    )
    .expect_err("already resolved");
    assert!(err.to_string().contains("ya está resuelta"), "{err}");
    assert_eq!(
        store
            .get_order(order.id)
            .expect("get")
            .expect("order")
            .status,
        OrderStatus::Filled
    );
}

#[test]
fn resolve_cancelled_and_partial_and_validation() {
    let order = unknown_order(Some(dec!(9700)));
    let mut store = store_with(&order);
    let fees = FeeModel::zero();
    // Out-of-range quantity and half-specified partial fills are refused.
    assert!(
        resolve_order(
            &mut store,
            &fees,
            now(),
            order.id,
            ResolveOutcome::Filled,
            Some(dec!(101)),
            None
        )
        .is_err()
    );
    assert!(
        resolve_order(
            &mut store,
            &fees,
            now(),
            order.id,
            ResolveOutcome::Cancelled,
            Some(dec!(10)),
            None
        )
        .is_err()
    );
    // Partly executed, the rest gone.
    resolve_order(
        &mut store,
        &fees,
        now(),
        order.id,
        ResolveOutcome::Cancelled,
        Some(dec!(40)),
        Some(dec!(9750)),
    )
    .expect("partial");
    let o = store.get_order(order.id).expect("get").expect("order");
    assert_eq!(o.status, OrderStatus::Cancelled);
    assert_eq!(o.filled_qty, dec!(40));

    // Plain cancellation.
    let order2 = unknown_order(Some(dec!(9700)));
    store.insert_order(&order2).expect("insert");
    resolve_order(
        &mut store,
        &fees,
        now(),
        order2.id,
        ResolveOutcome::Cancelled,
        None,
        None,
    )
    .expect("cancel");
    let o = store.get_order(order2.id).expect("get").expect("order");
    assert_eq!(o.status, OrderStatus::Cancelled);
    assert_eq!(o.filled_qty, Decimal::ZERO);
}

#[test]
fn market_order_needs_an_explicit_price_and_unknown_ids_fail() {
    let order = unknown_order(None);
    let mut store = store_with(&order);
    let fees = FeeModel::zero();
    let err = resolve_order(
        &mut store,
        &fees,
        now(),
        order.id,
        ResolveOutcome::Filled,
        None,
        None,
    )
    .expect_err("needs price");
    assert!(err.to_string().contains("--price"), "{err}");
    let err = resolve_order(
        &mut store,
        &fees,
        now(),
        Uuid::new_v4(),
        ResolveOutcome::Cancelled,
        None,
        None,
    )
    .expect_err("unknown id");
    assert!(err.to_string().contains("No existe"), "{err}");
    // Nothing was changed by the failed attempts.
    let o = store.get_order(order.id).expect("get").expect("order");
    assert_eq!(o.status, OrderStatus::Failed);
}

#[test]
fn cli_parses_orders_resolve() {
    let id = Uuid::new_v4().to_string();
    let cli = Cli::try_parse_from([
        "moon", "orders", "resolve", &id, "filled", "--qty", "5", "--price", "100.5",
    ])
    .expect("parse");
    match cli.command {
        Command::Orders {
            cmd:
                OrdersCmd::Resolve {
                    outcome,
                    qty,
                    price,
                    ..
                },
        } => {
            assert_eq!(outcome, ResolveOutcome::Filled);
            assert_eq!(qty, Some(dec!(5)));
            assert_eq!(price, Some(dec!(100.5)));
        }
        _ => panic!("expected orders resolve"),
    }
}

// --- N6: short ids, prefixes, listing ---

#[test]
fn short_id_is_the_first_eight_hex_characters() {
    let id = Uuid::parse_str("1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d").expect("uuid");
    assert_eq!(moon_app::engine::short_id(id), "1a2b3c4d");
}

#[test]
fn resolve_accepts_a_unique_prefix_of_six_or_more_characters() {
    let order = unknown_order(Some(dec!(9700)));
    let store = store_with(&order);
    let short = moon_app::engine::short_id(order.id);
    assert_eq!(
        find_order_id(&store, ExecMode::Live, &short).expect("short"),
        order.id
    );
    assert_eq!(
        find_order_id(&store, ExecMode::Live, &short[..6]).expect("six"),
        order.id
    );
    assert_eq!(
        find_order_id(&store, ExecMode::Live, &order.id.to_string().to_uppercase()).expect("full"),
        order.id
    );
    // Too short.
    let err = find_order_id(&store, ExecMode::Live, &short[..5]).expect_err("short prefix");
    assert!(err.to_string().contains("al menos 6"), "{err}");
    // Unknown prefix (valid hex, no order) and non-hex input.
    let other = if short.starts_with('0') {
        "ffffff"
    } else {
        "000000"
    };
    let err = find_order_id(&store, ExecMode::Live, other).expect_err("unknown");
    assert!(err.to_string().contains("No existe"), "{err}");
    let err = find_order_id(&store, ExecMode::Live, "%%%%%%").expect_err("wildcards");
    assert!(err.to_string().contains("No existe"), "{err}");
}

#[test]
fn ambiguous_prefix_lists_the_candidates() {
    let mut a = unknown_order(Some(dec!(9700)));
    let mut b = unknown_order(Some(dec!(9700)));
    a.id = Uuid::parse_str("1a2b3c4d-5e6f-4a7b-8c9d-000000000001").expect("uuid");
    b.id = Uuid::parse_str("1a2b3c4d-5e6f-4a7b-8c9d-000000000002").expect("uuid");
    let mut store = store_with(&a);
    store.insert_order(&b).expect("insert");
    let err = find_order_id(&store, ExecMode::Live, "1a2b3c").expect_err("ambiguous");
    let msg = err.to_string();
    assert!(msg.contains("ambiguo"), "{msg}");
    assert_eq!(msg.matches("AAA").count(), 2, "{msg}");
    // The full id still resolves.
    assert_eq!(
        find_order_id(&store, ExecMode::Live, &a.id.to_string()).expect("full"),
        a.id
    );
}

#[test]
fn orders_list_shows_open_and_unknown_orders_with_short_ids() {
    let unknown = unknown_order(Some(dec!(9700)));
    let mut open = unknown_order(Some(dec!(9700)));
    open.status = OrderStatus::Sent;
    open.error = None;
    let mut done = unknown_order(Some(dec!(9700)));
    done.status = OrderStatus::Filled;
    done.error = None;
    let mut store = store_with(&unknown);
    store.insert_order(&open).expect("insert");
    store.insert_order(&done).expect("insert");

    let all = list_text(&store, ExecMode::Live, false).expect("list");
    assert!(all.contains(&short_id(unknown.id)), "{all}");
    assert!(all.contains(&short_id(open.id)), "{all}");
    assert!(!all.contains(&short_id(done.id)), "{all}");

    let only_unknown = list_text(&store, ExecMode::Live, true).expect("list");
    assert!(only_unknown.contains(&short_id(unknown.id)));
    assert!(!only_unknown.contains(&short_id(open.id)));

    let empty = list_text(&store, ExecMode::Paper, false).expect("list");
    assert!(empty.contains("No hay"), "{empty}");
}

#[test]
fn cli_parses_orders_list_and_prefix_resolve() {
    let cli = Cli::try_parse_from(["moon", "orders", "list", "--unknown"]).expect("parse");
    assert!(matches!(
        cli.command,
        Command::Orders {
            cmd: OrdersCmd::List { unknown: true }
        }
    ));
    let cli =
        Cli::try_parse_from(["moon", "orders", "resolve", "1a2b3c", "cancelled"]).expect("parse");
    match cli.command {
        Command::Orders {
            cmd: OrdersCmd::Resolve { order_id, .. },
        } => assert_eq!(order_id, "1a2b3c"),
        _ => panic!("expected orders resolve"),
    }
}

#[test]
fn prefix_lookup_only_sees_orders_of_the_configured_mode() {
    let mut paper = unknown_order(Some(dec!(9700)));
    paper.mode = ExecMode::Paper;
    let live = unknown_order(Some(dec!(9700)));
    let mut store = store_with(&paper);
    store.insert_order(&live).expect("insert");
    let paper_short = moon_app::engine::short_id(paper.id);
    let live_short = moon_app::engine::short_id(live.id);
    // In live a paper order's prefix matches nothing.
    let err = find_order_id(&store, ExecMode::Live, &paper_short).expect_err("paper hidden");
    assert!(err.to_string().contains("No existe"), "{err}");
    assert_eq!(
        find_order_id(&store, ExecMode::Live, &live_short).expect("live"),
        live.id
    );
    // And the other way round in paper mode.
    assert_eq!(
        find_order_id(&store, ExecMode::Paper, &paper_short).expect("paper"),
        paper.id
    );
    assert!(find_order_id(&store, ExecMode::Paper, &live_short).is_err());
}
