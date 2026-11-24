//! Position reconciliation and deterministic order ids.

use std::collections::{BTreeMap, BTreeSet};

use moon_core::asset::AssetId;
use moon_core::types::Side;
use moon_store::OrderRecord;
use rust_decimal::Decimal;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub struct Discrepancy {
    pub symbol: String,
    pub expected: Decimal,
    pub actual: Decimal,
}

/// Compares the positions the bot expects (previous snapshot plus today's fills) with the
/// broker's current positions. Symbols are returned in alphabetical order.
pub fn reconcile_positions(
    previous: &BTreeMap<String, Decimal>,
    filled_today: &[OrderRecord],
    current: &BTreeMap<String, Decimal>,
) -> Vec<Discrepancy> {
    let mut expected = previous.clone();
    for o in filled_today {
        if o.filled_qty <= Decimal::ZERO {
            continue;
        }
        let entry = expected.entry(o.symbol.clone()).or_default();
        match o.side {
            Side::Buy => *entry += o.filled_qty,
            Side::Sell => *entry -= o.filled_qty,
        }
    }
    let symbols: BTreeSet<&String> = expected.keys().chain(current.keys()).collect();
    symbols
        .into_iter()
        .filter_map(|s| {
            let e = expected.get(s).copied().unwrap_or(Decimal::ZERO);
            let a = current.get(s).copied().unwrap_or(Decimal::ZERO);
            (e != a).then(|| Discrepancy {
                symbol: s.clone(),
                expected: e,
                actual: a,
            })
        })
        .collect()
}

/// Idempotency key of an order: UUID v5 of the batch id over `"<asset>:<side>"`.
pub fn order_id_for(batch_id: Uuid, asset: &AssetId, side: Side) -> Uuid {
    Uuid::new_v5(&batch_id, format!("{asset}:{}", side.as_str()).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use moon_core::asset::{Currency, Market};
    use moon_core::types::{ExecMode, OrderReason, OrderStatus};
    use rust_decimal_macros::dec;

    fn filled(symbol: &str, side: Side, qty: Decimal) -> OrderRecord {
        OrderRecord {
            id: Uuid::new_v4(),
            batch_id: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            mode: ExecMode::Paper,
            reason: OrderReason::Monthly,
            asset: AssetId::new(symbol),
            symbol: symbol.to_string(),
            market: Market::Bcba,
            side,
            qty,
            limit_price: None,
            currency: Currency::Ars,
            status: OrderStatus::Filled,
            broker_order_id: None,
            filled_qty: qty,
            avg_fill_price: None,
            fees: Decimal::ZERO,
            fees_estimated: false,
            error: None,
        }
    }

    #[test]
    fn reconcile_detects_mismatch() {
        let prev = BTreeMap::from([("AAA".to_string(), dec!(10)), ("BBB".to_string(), dec!(5))]);
        let orders = vec![
            filled("AAA", Side::Buy, dec!(4)),
            filled("BBB", Side::Sell, dec!(5)),
        ];
        let ok = BTreeMap::from([("AAA".to_string(), dec!(14))]);
        assert!(reconcile_positions(&prev, &orders, &ok).is_empty());

        let bad = BTreeMap::from([("AAA".to_string(), dec!(13)), ("CCC".to_string(), dec!(1))]);
        let d = reconcile_positions(&prev, &orders, &bad);
        assert_eq!(
            d,
            vec![
                Discrepancy {
                    symbol: "AAA".into(),
                    expected: dec!(14),
                    actual: dec!(13)
                },
                Discrepancy {
                    symbol: "CCC".into(),
                    expected: dec!(0),
                    actual: dec!(1)
                },
            ]
        );
    }

    #[test]
    fn order_ids_are_deterministic_and_distinct() {
        let b = Uuid::new_v4();
        let a = AssetId::new("AAA");
        assert_eq!(
            order_id_for(b, &a, Side::Buy),
            order_id_for(b, &a, Side::Buy)
        );
        assert_ne!(
            order_id_for(b, &a, Side::Buy),
            order_id_for(b, &a, Side::Sell)
        );
        assert_ne!(
            order_id_for(b, &a, Side::Buy),
            order_id_for(Uuid::new_v4(), &a, Side::Buy)
        );
    }
}
