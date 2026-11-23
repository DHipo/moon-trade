//! Turns target weights into concrete orders without ever overspending cash.

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::asset::AssetId;
use crate::error::CoreError;
use crate::fees::FeeModel;
use crate::money::{floor_to_lot, round_money, round_price, weight_to_dec};
use crate::types::Side;
use crate::weights::TargetWeights;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RebalanceParams {
    pub band: f64,
    pub min_trade_value: Decimal,
    pub cash_buffer: f64,
    pub slippage_rate: Decimal,
    pub force_full: bool,
}

pub struct RebalanceInput<'a> {
    pub cash: Decimal,
    pub holdings: &'a BTreeMap<AssetId, Decimal>,
    pub prices: &'a BTreeMap<AssetId, Decimal>,
    /// Missing means 1.
    pub lot_sizes: &'a BTreeMap<AssetId, u32>,
    /// May contain CASH, which is never traded.
    pub targets: &'a TargetWeights,
    pub fees: &'a FeeModel,
    pub params: &'a RebalanceParams,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedOrder {
    pub asset: AssetId,
    pub side: Side,
    pub qty: Decimal,
    pub ref_price: Decimal,
    pub exec_price: Decimal,
    pub est_value: Decimal,
    pub est_fees: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    WithinBand,
    BelowMinTrade,
    InsufficientCash,
    LotRounding,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedTrade {
    pub asset: AssetId,
    pub side: Side,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RebalancePlan {
    pub equity: Decimal,
    pub orders: Vec<PlannedOrder>,
    pub skipped: Vec<SkippedTrade>,
    pub est_cash_after: Decimal,
}

impl RebalancePlan {
    pub fn sells(&self) -> Vec<&PlannedOrder> {
        self.orders
            .iter()
            .filter(|o| o.side == Side::Sell)
            .collect()
    }

    pub fn buys(&self) -> Vec<&PlannedOrder> {
        self.orders.iter().filter(|o| o.side == Side::Buy).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }
}

struct Buy {
    asset: AssetId,
    ref_price: Decimal,
    exec: Decimal,
    lot: u32,
    qty: Decimal,
    value: Decimal,
    fees: Decimal,
}

impl Buy {
    fn cost(&self) -> Decimal {
        self.value + self.fees
    }

    fn reprice(&mut self, fees: &FeeModel) {
        self.value = round_money(self.qty * self.exec);
        self.fees = fees.total(self.value);
    }
}

pub fn plan_rebalance(input: &RebalanceInput<'_>) -> Result<RebalancePlan, CoreError> {
    let p = input.params;
    let cash = input.cash;
    if cash < Decimal::ZERO {
        return Err(CoreError::InvalidConfig(
            "el efectivo no puede ser negativo".into(),
        ));
    }
    if !(p.band.is_finite() && (0.0..0.5).contains(&p.band)) {
        return Err(CoreError::InvalidConfig(
            "band debe estar en [0, 0.5)".into(),
        ));
    }
    if !(p.cash_buffer.is_finite() && (0.0..=0.2).contains(&p.cash_buffer)) {
        return Err(CoreError::InvalidConfig(
            "cash_buffer debe estar en [0, 0.2]".into(),
        ));
    }

    if !(p.slippage_rate >= Decimal::ZERO && p.slippage_rate < Decimal::new(5, 1)) {
        return Err(CoreError::InvalidConfig(
            "slippage_rate debe estar en [0, 0.5)".into(),
        ));
    }

    // Assets to consider: held or targeted, excluding CASH.
    let mut universe: BTreeSet<&AssetId> = BTreeSet::new();
    for (id, q) in input.holdings {
        if !id.is_cash() && *q > Decimal::ZERO {
            universe.insert(id);
        }
    }
    for (id, w) in input.targets.iter() {
        if !id.is_cash() && w > 0.0 {
            universe.insert(id);
        }
    }
    let mut priced: BTreeMap<&AssetId, Decimal> = BTreeMap::new();
    for id in &universe {
        match input.prices.get(*id) {
            Some(px) if *px > Decimal::ZERO => {
                priced.insert(*id, *px);
            }
            _ => return Err(CoreError::MissingPrice((*id).clone())),
        }
    }

    let held = |id: &AssetId| {
        input
            .holdings
            .get(id)
            .copied()
            .unwrap_or(Decimal::ZERO)
            .max(Decimal::ZERO)
    };
    let mut equity = cash;
    for (id, px) in &priced {
        equity += held(id) * *px;
    }
    if equity <= Decimal::ZERO {
        return Ok(RebalancePlan {
            equity,
            orders: Vec::new(),
            skipped: Vec::new(),
            est_cash_after: cash,
        });
    }

    let investable = equity * (Decimal::ONE - weight_to_dec(p.cash_buffer));
    let band = weight_to_dec(p.band);
    let deploy = cash / equity > weight_to_dec(p.cash_buffer + p.band);

    let mut sells: Vec<PlannedOrder> = Vec::new();
    let mut buys: Vec<Buy> = Vec::new();
    let mut skipped: Vec<SkippedTrade> = Vec::new();
    let mut proceeds = Decimal::ZERO;

    for (id, price) in &priced {
        let id = *id;
        let price = *price;
        let lot = input.lot_sizes.get(id).copied().unwrap_or(1);
        let w = weight_to_dec(input.targets.get(id));
        let qty_held = held(id);
        let cur = qty_held * price;
        let tgt = investable * w;
        let dev = cur / equity - w;
        let side = if tgt < cur { Side::Sell } else { Side::Buy };

        let trade = p.force_full
            || (w == Decimal::ZERO && qty_held > Decimal::ZERO)
            || (qty_held == Decimal::ZERO && w > Decimal::ZERO)
            || dev.abs() > band
            || (deploy && tgt > cur);
        if !trade {
            if tgt != cur {
                skipped.push(SkippedTrade {
                    asset: id.clone(),
                    side,
                    reason: SkipReason::WithinBand,
                });
            }
            continue;
        }
        if tgt == cur {
            continue;
        }
        let skip = |reason| SkippedTrade {
            asset: id.clone(),
            side,
            reason,
        };

        if tgt < cur {
            let exec = round_price(price * (Decimal::ONE - p.slippage_rate));
            let qty = if w == Decimal::ZERO {
                qty_held
            } else {
                qty_held.min(floor_to_lot((cur - tgt) / price, lot))
            };
            if exec <= Decimal::ZERO {
                skipped.push(skip(SkipReason::BelowMinTrade));
                continue;
            }
            if qty <= Decimal::ZERO {
                skipped.push(skip(SkipReason::LotRounding));
                continue;
            }
            let value = round_money(qty * exec);
            if value < p.min_trade_value && w != Decimal::ZERO {
                skipped.push(skip(SkipReason::BelowMinTrade));
                continue;
            }
            let fees = input.fees.total(value);
            // Dust: fees eat the whole sale (applies to full exits too); keep it and report it.
            if fees >= value {
                skipped.push(skip(SkipReason::BelowMinTrade));
                continue;
            }
            proceeds += value - fees;
            sells.push(PlannedOrder {
                asset: id.clone(),
                side: Side::Sell,
                qty,
                ref_price: price,
                exec_price: exec,
                est_value: value,
                est_fees: fees,
            });
        } else {
            let exec = round_price(price * (Decimal::ONE + p.slippage_rate));
            if exec <= Decimal::ZERO {
                skipped.push(skip(SkipReason::BelowMinTrade));
                continue;
            }
            let qty = floor_to_lot((tgt - cur) / exec, lot);
            if qty <= Decimal::ZERO {
                skipped.push(skip(SkipReason::LotRounding));
                continue;
            }
            let value = round_money(qty * exec);
            if value < p.min_trade_value {
                skipped.push(skip(SkipReason::BelowMinTrade));
                continue;
            }
            buys.push(Buy {
                asset: id.clone(),
                ref_price: price,
                exec,
                lot,
                qty,
                value,
                fees: input.fees.total(value),
            });
        }
    }

    let budget = cash + proceeds;
    let total_cost = |b: &[Buy]| b.iter().map(Buy::cost).sum::<Decimal>();
    let drop_buy = |b: &Buy, skipped: &mut Vec<SkippedTrade>| {
        skipped.push(SkippedTrade {
            asset: b.asset.clone(),
            side: Side::Buy,
            reason: SkipReason::InsufficientCash,
        });
    };

    let cost = total_cost(&buys);
    if cost > budget {
        let factor = if budget > Decimal::ZERO {
            budget / cost
        } else {
            Decimal::ZERO
        };
        let mut kept = Vec::with_capacity(buys.len());
        for mut b in buys {
            b.qty = floor_to_lot(b.qty * factor, b.lot);
            b.reprice(input.fees);
            if b.qty <= Decimal::ZERO || b.value < p.min_trade_value {
                drop_buy(&b, &mut skipped);
            } else {
                kept.push(b);
            }
        }
        buys = kept;
        while !buys.is_empty() && total_cost(&buys) > budget {
            let mut idx = 0;
            for (i, b) in buys.iter().enumerate() {
                if b.value < buys[idx].value {
                    idx = i;
                }
            }
            let b = buys.remove(idx);
            drop_buy(&b, &mut skipped);
        }
    }

    let est_cash_after = budget - total_cost(&buys);
    // Unreachable: every emitted sell has positive net proceeds and buys fit the budget.
    if est_cash_after < Decimal::ZERO {
        return Err(CoreError::InvalidConfig(
            "el plan dejaría el efectivo en negativo".into(),
        ));
    }

    let mut orders = sells;
    orders.extend(buys.into_iter().map(|b| PlannedOrder {
        asset: b.asset,
        side: Side::Buy,
        qty: b.qty,
        ref_price: b.ref_price,
        exec_price: b.exec,
        est_value: b.value,
        est_fees: b.fees,
    }));
    Ok(RebalancePlan {
        equity,
        orders,
        skipped,
        est_cash_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal_macros::dec;

    fn id(s: &str) -> AssetId {
        AssetId::new(s)
    }

    fn params() -> RebalanceParams {
        RebalanceParams {
            band: 0.05,
            min_trade_value: dec!(0),
            cash_buffer: 0.01,
            slippage_rate: dec!(0),
            force_full: false,
        }
    }

    fn map(items: &[(&str, Decimal)]) -> BTreeMap<AssetId, Decimal> {
        items.iter().map(|(k, v)| (id(k), *v)).collect()
    }

    fn targets(items: &[(&str, f64)]) -> TargetWeights {
        let mut t = TargetWeights::new();
        for (k, w) in items {
            t.add(id(k), *w);
        }
        t
    }

    fn run(
        cash: Decimal,
        holdings: &BTreeMap<AssetId, Decimal>,
        prices: &BTreeMap<AssetId, Decimal>,
        lots: &BTreeMap<AssetId, u32>,
        t: &TargetWeights,
        fees: &FeeModel,
        p: &RebalanceParams,
    ) -> Result<RebalancePlan, CoreError> {
        plan_rebalance(&RebalanceInput {
            cash,
            holdings,
            prices,
            lot_sizes: lots,
            targets: t,
            fees,
            params: p,
        })
    }

    #[test]
    fn initial_deployment_exact() {
        let prices = map(&[("SPY", dec!(100)), ("QQQ", dec!(50))]);
        let plan = run(
            dec!(10000),
            &BTreeMap::new(),
            &prices,
            &BTreeMap::new(),
            &targets(&[("SPY", 0.5), ("QQQ", 0.5)]),
            &FeeModel::zero(),
            &params(),
        )
        .unwrap();
        let q = |a: &str| plan.orders.iter().find(|o| o.asset == id(a)).map(|o| o.qty);
        assert_eq!(q("SPY"), Some(dec!(49)));
        assert_eq!(q("QQQ"), Some(dec!(99)));
        assert_eq!(plan.est_cash_after, dec!(150));
        assert_eq!(plan.equity, dec!(10000));
    }

    #[test]
    fn within_band_no_orders() {
        let prices = map(&[("SPY", dec!(100)), ("QQQ", dec!(50))]);
        let holdings = map(&[("SPY", dec!(51)), ("QQQ", dec!(98))]);
        let mut p = params();
        p.cash_buffer = 0.0;
        let plan = run(
            dec!(0),
            &holdings,
            &prices,
            &BTreeMap::new(),
            &targets(&[("SPY", 0.5), ("QQQ", 0.5)]),
            &FeeModel::zero(),
            &p,
        )
        .unwrap();
        assert!(plan.is_empty());
        assert_eq!(plan.skipped.len(), 2);
        assert!(
            plan.skipped
                .iter()
                .all(|s| s.reason == SkipReason::WithinBand)
        );
    }

    #[test]
    fn full_exit_when_weight_zero() {
        let prices = map(&[("A", dec!(1))]);
        let holdings = map(&[("A", dec!(10))]);
        let mut p = params();
        p.min_trade_value = dec!(100);
        let plan = run(
            dec!(1000),
            &holdings,
            &prices,
            &BTreeMap::new(),
            &targets(&[("CASH", 1.0)]),
            &FeeModel::zero(),
            &p,
        )
        .unwrap();
        assert_eq!(plan.orders.len(), 1);
        assert_eq!(plan.orders[0].side, Side::Sell);
        assert_eq!(plan.orders[0].qty, dec!(10));
        assert_eq!(plan.est_cash_after, dec!(1010));
    }

    #[test]
    fn sells_listed_before_buys() {
        let prices = map(&[("A", dec!(10)), ("B", dec!(10))]);
        let holdings = map(&[("A", dec!(100))]);
        let plan = run(
            dec!(0),
            &holdings,
            &prices,
            &BTreeMap::new(),
            &targets(&[("B", 1.0)]),
            &FeeModel::zero(),
            &params(),
        )
        .unwrap();
        assert_eq!(plan.orders.len(), 2);
        assert_eq!(plan.orders[0].side, Side::Sell);
        assert_eq!(plan.orders[1].side, Side::Buy);
        assert_eq!(plan.sells().len(), 1);
        assert_eq!(plan.buys().len(), 1);
        assert!(plan.est_cash_after >= dec!(0));
    }

    #[test]
    fn missing_price_is_error() {
        let r = run(
            dec!(1000),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &targets(&[("A", 1.0)]),
            &FeeModel::zero(),
            &params(),
        );
        assert_eq!(r, Err(CoreError::MissingPrice(id("A"))));
        let r = run(
            dec!(1000),
            &BTreeMap::new(),
            &map(&[("A", dec!(0))]),
            &BTreeMap::new(),
            &targets(&[("A", 1.0)]),
            &FeeModel::zero(),
            &params(),
        );
        assert_eq!(r, Err(CoreError::MissingPrice(id("A"))));
    }

    #[test]
    fn invalid_params_are_errors() {
        let prices = map(&[("A", dec!(1))]);
        let t = targets(&[("A", 1.0)]);
        let e = BTreeMap::new();
        let f = FeeModel::zero();
        let mut p = params();
        p.band = 0.5;
        assert!(matches!(
            run(dec!(1), &e, &prices, &BTreeMap::new(), &t, &f, &p),
            Err(CoreError::InvalidConfig(_))
        ));
        let mut p = params();
        p.cash_buffer = 0.3;
        assert!(run(dec!(1), &e, &prices, &BTreeMap::new(), &t, &f, &p).is_err());
        assert!(run(dec!(-1), &e, &prices, &BTreeMap::new(), &t, &f, &params()).is_err());
    }

    #[test]
    fn lot_size_respected() {
        let prices = map(&[("A", dec!(7))]);
        let lots: BTreeMap<AssetId, u32> = [(id("A"), 5)].into_iter().collect();
        let plan = run(
            dec!(10000),
            &BTreeMap::new(),
            &prices,
            &lots,
            &targets(&[("A", 1.0)]),
            &FeeModel::zero(),
            &params(),
        )
        .unwrap();
        assert_eq!(plan.orders[0].qty, dec!(1410));
        assert_eq!(plan.orders[0].qty % dec!(5), dec!(0));
    }

    #[test]
    fn deposit_is_deployed() {
        let prices = map(&[("SPY", dec!(100)), ("QQQ", dec!(50))]);
        let holdings = map(&[("SPY", dec!(100)), ("QQQ", dec!(200))]);
        let plan = run(
            dec!(5000),
            &holdings,
            &prices,
            &BTreeMap::new(),
            &targets(&[("SPY", 0.5), ("QQQ", 0.5)]),
            &FeeModel::zero(),
            &params(),
        )
        .unwrap();
        assert!(!plan.is_empty());
        assert!(plan.sells().is_empty());
        assert_eq!(plan.buys().len(), 2);
    }

    #[test]
    fn insufficient_cash_scales_buys() {
        let prices = map(&[("SPY", dec!(100)), ("QQQ", dec!(50))]);
        let fees = FeeModel {
            commission_rate: dec!(0.006),
            market_fee_rate: dec!(0.0008),
            vat_rate: dec!(0.21),
            min_commission: dec!(400),
        };
        let plan = run(
            dec!(1000),
            &BTreeMap::new(),
            &prices,
            &BTreeMap::new(),
            &targets(&[("SPY", 0.5), ("QQQ", 0.5)]),
            &fees,
            &params(),
        )
        .unwrap();
        assert!(plan.est_cash_after >= dec!(0));
        assert!(
            plan.skipped
                .iter()
                .any(|s| s.reason == SkipReason::InsufficientCash)
        );
        let spent: Decimal = plan.buys().iter().map(|o| o.est_value + o.est_fees).sum();
        assert!(spent <= dec!(1000));
        assert_eq!(plan.buys().len(), 1);
        assert_eq!(plan.buys()[0].asset, id("SPY"));
        assert_eq!(plan.buys()[0].qty, dec!(2));
    }

    #[test]
    fn force_full_exit_with_tiny_position_and_high_min_commission() {
        let prices = map(&[("BIG", dec!(100)), ("TINY", dec!(1))]);
        let holdings = map(&[("BIG", dec!(100)), ("TINY", dec!(3))]);
        let fees = FeeModel {
            commission_rate: dec!(0.006),
            market_fee_rate: dec!(0.0008),
            vat_rate: dec!(0.21),
            min_commission: dec!(50),
        };
        let mut p = params();
        p.force_full = true;
        let plan = run(
            dec!(0),
            &holdings,
            &prices,
            &BTreeMap::new(),
            &targets(&[("CASH", 1.0)]),
            &fees,
            &p,
        )
        .unwrap();
        assert_eq!(plan.sells().len(), 1);
        assert_eq!(plan.sells()[0].asset, id("BIG"));
        assert!(plan.skipped.iter().any(|s| s.asset == id("TINY")
            && s.side == Side::Sell
            && s.reason == SkipReason::BelowMinTrade));
        assert!(plan.est_cash_after >= dec!(0));
    }

    #[test]
    fn invalid_slippage_is_error() {
        let prices = map(&[("A", dec!(1))]);
        let mut p = params();
        p.slippage_rate = dec!(0.5);
        let r = run(
            dec!(1),
            &BTreeMap::new(),
            &prices,
            &BTreeMap::new(),
            &targets(&[("A", 1.0)]),
            &FeeModel::zero(),
            &p,
        );
        assert!(matches!(r, Err(CoreError::InvalidConfig(_))));
    }

    #[test]
    fn emergency_force_full_sells_all_risky() {
        let prices = map(&[("SPY", dec!(100)), ("QQQ", dec!(50)), ("SHY", dec!(80))]);
        let holdings = map(&[("SPY", dec!(50)), ("QQQ", dec!(100))]);
        let mut p = params();
        p.force_full = true;
        p.cash_buffer = 0.0;
        let plan = run(
            dec!(0),
            &holdings,
            &prices,
            &BTreeMap::new(),
            &targets(&[("SHY", 1.0)]),
            &FeeModel::zero(),
            &p,
        )
        .unwrap();
        assert_eq!(plan.sells().len(), 2);
        assert_eq!(plan.sells()[0].qty, dec!(100));
        assert_eq!(plan.sells()[1].qty, dec!(50));
        assert_eq!(plan.buys().len(), 1);
        assert_eq!(plan.buys()[0].asset, id("SHY"));
        assert_eq!(plan.buys()[0].qty, dec!(125));
    }

    proptest! {
        #[test]
        fn plan_invariants(
            cash in prop_oneof![Just(0u32), 0u32..100, 0u32..1_000_000],
            force_full in any::<bool>(),
            assets in prop::collection::vec(
                (1u32..1000, 0u32..500, 0.0f64..1.0, prop::sample::select(vec![1u32, 5, 10])),
                1..=4,
            ),
            cash_w in prop_oneof![Just(0.0f64), 0.0f64..1.0],
            band in 0.0f64..0.1,
            buffer in 0.0f64..0.05,
            rates in (0u32..100, 0u32..100, 0u32..100, 0u32..50, 0u32..50),
        ) {
            let (slip, comm, mkt, min_comm, min_trade) = rates;
            let mut holdings = BTreeMap::new();
            let mut prices = BTreeMap::new();
            let mut lots = BTreeMap::new();
            let mut raw = Vec::new();
            for (i, (px, qty, w, lot)) in assets.iter().enumerate() {
                let a = id(&format!("A{i}"));
                prices.insert(a.clone(), Decimal::from(*px));
                holdings.insert(a.clone(), Decimal::from(*qty));
                lots.insert(a.clone(), *lot);
                raw.push((a, *w));
            }
            let total: f64 = raw.iter().map(|(_, w)| *w).sum::<f64>() + cash_w;
            let mut t = TargetWeights::new();
            if total > 0.0 {
                for (a, w) in &raw {
                    t.add(a.clone(), w / total);
                }
                t.add(AssetId::cash(), cash_w / total);
            } else {
                t.add(AssetId::cash(), 1.0);
            }
            let fees = FeeModel {
                commission_rate: Decimal::new(i64::from(comm), 4),
                market_fee_rate: Decimal::new(i64::from(mkt), 4),
                vat_rate: dec!(0.21),
                min_commission: Decimal::from(min_comm),
            };
            let p = RebalanceParams {
                band,
                min_trade_value: Decimal::from(min_trade),
                cash_buffer: buffer,
                slippage_rate: Decimal::new(i64::from(slip), 4),
                force_full,
            };
            let cash = Decimal::from(cash);
            let plan = run(cash, &holdings, &prices, &lots, &t, &fees, &p)
                .map_err(|e| TestCaseError::fail(format!("plan must be Ok: {e}")))?;
            prop_assert!(plan.est_cash_after >= Decimal::ZERO);
            let buy_cost: Decimal = plan.buys().iter().map(|o| o.est_value + o.est_fees).sum();
            let sell_net: Decimal = plan.sells().iter().map(|o| o.est_value - o.est_fees).sum();
            prop_assert!(buy_cost <= cash + sell_net);
            for o in &plan.orders {
                let held = holdings.get(&o.asset).copied().unwrap_or_default();
                let lot = Decimal::from(lots[&o.asset]);
                prop_assert!(o.qty > Decimal::ZERO);
                if o.side == Side::Sell {
                    prop_assert!(o.qty <= held);
                }
                // Only full exits (target weight 0) may sell a non-lot-multiple holding.
                let full_exit =
                    o.side == Side::Sell && weight_to_dec(t.get(&o.asset)) == Decimal::ZERO;
                if o.side == Side::Sell {
                    prop_assert!(o.est_value > o.est_fees);
                }
                prop_assert!(full_exit || o.qty % lot == Decimal::ZERO);
                prop_assert!(held > Decimal::ZERO || t.get(&o.asset) > 0.0);
            }
        }
    }
}
