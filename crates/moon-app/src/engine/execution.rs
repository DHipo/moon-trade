//! Order execution: sells first, then buys recomputed from the broker's real cash.

use std::time::{Duration, Instant};

use std::collections::BTreeSet;

use anyhow::{anyhow, bail};
use moon_broker::{OrderProgress, OrderTicket};
use moon_core::asset::AssetId;
use moon_core::money::{floor_to_lot, round_money, round_price};
use moon_core::rebalance::{PlannedOrder, RebalancePlan};
use moon_core::risk::check_orders;
use moon_core::time::art_date;
use moon_core::types::{OrderReason, OrderStatus, OrderType, Side};
use moon_core::weights::TargetWeights;
use moon_store::OrderRecord;
use rust_decimal::Decimal;
use uuid::Uuid;

use super::reconcile::order_id_for_tranche;
use super::text::execution_summary;
use super::{Engine, ExecutionReport, ORDER_BLOCKED, UNKNOWN_OUTCOME, art_day_bounds};

/// Smallest multiple of `tick` that is `>= price`.
pub fn ceil_to_tick(price: Decimal, tick: Decimal) -> Decimal {
    if tick <= Decimal::ZERO {
        return round_price(price);
    }
    round_price((price / tick).ceil() * tick)
}

/// Largest multiple of `tick` that is `<= price`.
pub fn floor_to_tick(price: Decimal, tick: Decimal) -> Decimal {
    if tick <= Decimal::ZERO {
        return round_price(price);
    }
    round_price((price / tick).floor() * tick)
}

struct SendOutcome {
    id: Uuid,
    /// Set when the batch must stop (unknown outcome or a previously unknown order).
    halt: Option<String>,
}

impl Engine {
    /// Executes a plan: the sells of `plan` first, then buys recomputed from real cash.
    ///
    /// Orders are stored before they are sent, with an id derived from the batch, so running the
    /// same batch again never places an order twice. Does not take the trading lock: callers do.
    pub async fn execute(
        &self,
        plan: &RebalancePlan,
        targets: &TargetWeights,
        reason: OrderReason,
        batch_id: Uuid,
        emergency: bool,
    ) -> anyhow::Result<ExecutionReport> {
        self.execute_excluding(plan, targets, reason, batch_id, emergency, &BTreeSet::new())
            .await
    }

    /// Like [`Engine::execute`], never trading the `excluded` assets (they already have a working
    /// order or an order with unknown outcome): neither the plan's orders nor the buys that are
    /// recomputed from real cash include them.
    pub(crate) async fn execute_excluding(
        &self,
        plan: &RebalancePlan,
        targets: &TargetWeights,
        reason: OrderReason,
        batch_id: Uuid,
        emergency: bool,
        excluded: &BTreeSet<AssetId>,
    ) -> anyhow::Result<ExecutionReport> {
        self.execute_inner(plan, targets, reason, batch_id, emergency, excluded, false)
            .await
    }

    /// Buys the refuge after an emergency exit: the normal tranche splitting applies, but the
    /// daily order limit does not (getting into the refuge must not be blocked by the sells and
    /// earlier orders of the day).
    pub(crate) async fn execute_refuge_buy(
        &self,
        plan: &RebalancePlan,
        targets: &TargetWeights,
        batch_id: Uuid,
    ) -> anyhow::Result<ExecutionReport> {
        self.execute_inner(
            plan,
            targets,
            OrderReason::Emergency,
            batch_id,
            false,
            &BTreeSet::new(),
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_inner(
        &self,
        plan: &RebalancePlan,
        targets: &TargetWeights,
        reason: OrderReason,
        batch_id: Uuid,
        emergency: bool,
        excluded: &BTreeSet<AssetId>,
        exempt_daily_limit: bool,
    ) -> anyhow::Result<ExecutionReport> {
        let max_orders_per_day = if exempt_daily_limit {
            u32::MAX
        } else {
            self.cfg.risk.max_orders_per_day
        };
        // Emergency exits never depend on the CCL (their order checks ignore the size limit).
        let ccl = self
            .db(|s| s.latest_fx())?
            .map(|fx| fx.rate)
            .filter(|r| *r > Decimal::ZERO);
        let ccl = match (ccl, emergency) {
            (Some(c), _) => c,
            (None, true) => Decimal::ONE,
            (None, false) => {
                bail!("no hay tipo de cambio registrado: valuá la cartera primero")
            }
        };

        // An emergency exit only ever sells; buys of the plan (e.g. a non-cash refuge) are dropped.
        let first_phase: Vec<PlannedOrder> = if emergency {
            plan.sells().into_iter().cloned().collect()
        } else {
            plan.orders
                .iter()
                .filter(|o| !excluded.contains(&o.asset))
                .cloned()
                .collect()
        };
        let mut errors: Vec<String> = Vec::new();
        let first_phase = self.cap_sells(first_phase, &mut errors).await?;
        // An order above `max_order_usd` is split into tranches instead of blocking the plan
        // (emergency exits ignore the size limit).
        let first_phase = if emergency {
            first_phase.into_iter().map(|o| (o, 0)).collect()
        } else {
            self.split_oversize(first_phase, ccl)
        };
        let already = self.orders_today_outside(batch_id)?;
        let checked: Vec<PlannedOrder> = first_phase.iter().map(|(o, _)| o.clone()).collect();
        if let Err(violation) = check_orders(
            &checked,
            round_money(self.cfg.risk.max_order_usd * ccl),
            max_orders_per_day,
            already,
            emergency,
        ) {
            let msg = format!("{ORDER_BLOCKED}: {violation}");
            self.deps.notifier.notify(&format!("⛔ {msg}")).await;
            bail!(msg);
        }

        let mut ids: Vec<Uuid> = Vec::new();
        let mut halted = false;

        // Phase 1: sells.
        let sells: Vec<&(PlannedOrder, u32)> = first_phase
            .iter()
            .filter(|(o, _)| o.side == Side::Sell)
            .collect();
        for (order, tranche) in sells {
            let out = self
                .send_order(order, *tranche, reason, batch_id, emergency)
                .await?;
            ids.push(out.id);
            if let Some(msg) = out.halt {
                errors.push(msg);
                halted = true;
                break;
            }
        }
        // Emergency sells are not awaited here: the risk check waits for them afterwards (up to
        // `emergency_fill_wait_seconds`) so the refuge is bought as soon as they fill.
        if !halted && !ids.is_empty() && !emergency {
            self.wait_for_fills(&ids).await;
        }

        // Phase 2: buys recomputed from the broker's real cash.
        if !halted && !emergency {
            match self
                .recompute_buys(targets, batch_id, excluded, max_orders_per_day)
                .await
            {
                Ok((buys, cash)) => {
                    let mut cash_left = cash;
                    for (order, tranche) in &buys {
                        let cost = self.buy_cost(order);
                        if cost > cash_left {
                            errors.push(format!(
                                "Compra de {} omitida: efectivo insuficiente (ventas pendientes).",
                                order.asset
                            ));
                            continue;
                        }
                        let out = self
                            .send_order(order, *tranche, reason, batch_id, emergency)
                            .await?;
                        ids.push(out.id);
                        if let Some(msg) = out.halt {
                            errors.push(msg);
                            break;
                        }
                        cash_left -= cost;
                    }
                }
                Err(e) => errors.push(format!("No se pudieron calcular las compras: {e}")),
            }
        }

        let mut orders = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(o) = self.db(|s| s.get_order(*id))? {
                orders.push(o);
            }
        }
        let report = ExecutionReport {
            batch_id,
            orders,
            errors,
        };
        self.deps
            .notifier
            .notify(&execution_summary(reason, &report))
            .await;
        Ok(report)
    }

    /// Limit price of a non-emergency order: the reference price moved by the tolerance and
    /// rounded to the tick in the unfavourable direction for us.
    fn limit_price_for(&self, side: Side, ref_price: Decimal) -> Decimal {
        let tolerance = self.cfg.execution.limit_tolerance;
        let tick = self.cfg.execution.price_tick;
        match side {
            Side::Buy => ceil_to_tick(ref_price * (Decimal::ONE + tolerance), tick),
            Side::Sell => {
                let p = floor_to_tick(ref_price * (Decimal::ONE - tolerance), tick);
                if p > Decimal::ZERO { p } else { tick }
            }
        }
    }

    /// Cash a buy can consume: `qty x limit price + estimated fees`; market orders use the
    /// reference price moved by the limit tolerance.
    fn buy_cost(&self, order: &PlannedOrder) -> Decimal {
        let price = match self.cfg.execution.order_type {
            OrderType::Limit => self.limit_price_for(Side::Buy, order.ref_price),
            OrderType::Market => {
                order.ref_price * (Decimal::ONE + self.cfg.execution.limit_tolerance)
            }
        };
        let value = round_money(order.qty * price);
        value + self.cfg.fees.total(value)
    }

    /// Caps every sell at the broker's sellable quantity (floored to the lot size); a sell with
    /// nothing sellable is dropped and reported.
    async fn cap_sells(
        &self,
        orders: Vec<PlannedOrder>,
        errors: &mut Vec<String>,
    ) -> anyhow::Result<Vec<PlannedOrder>> {
        if !orders.iter().any(|o| o.side == Side::Sell) {
            return Ok(orders);
        }
        let account = self.deps.broker.account().await?;
        let mut out = Vec::with_capacity(orders.len());
        for mut o in orders {
            if o.side == Side::Sell {
                let Some(exec) = self.universe.get(&o.asset).and_then(|a| a.exec.as_ref()) else {
                    out.push(o);
                    continue;
                };
                let sellable = account
                    .sellable
                    .get(&exec.symbol)
                    .copied()
                    .unwrap_or(Decimal::ZERO);
                if o.qty > sellable {
                    let capped = floor_to_lot(sellable, exec.lot_size);
                    if capped <= Decimal::ZERO {
                        errors.push(format!(
                            "Venta de {} omitida: no hay cantidad vendible (disponible {sellable}).",
                            exec.symbol
                        ));
                        continue;
                    }
                    o.qty = capped;
                    o.est_value = round_money(capped * o.exec_price);
                    o.est_fees = self.cfg.fees.total(o.est_value);
                }
            }
            out.push(o);
        }
        Ok(out)
    }

    /// Fresh valuation and plan; returns only the buys (split into tranches, never the
    /// `excluded` assets), plus the real cash they can use.
    async fn recompute_buys(
        &self,
        targets: &TargetWeights,
        batch_id: Uuid,
        excluded: &BTreeSet<AssetId>,
        max_orders_per_day: u32,
    ) -> anyhow::Result<(Vec<(PlannedOrder, u32)>, Decimal)> {
        let v = self.valuation().await?;
        let plan = self.plan(targets, &v, false)?;
        let buys: Vec<PlannedOrder> = plan
            .buys()
            .into_iter()
            .filter(|o| !excluded.contains(&o.asset))
            .cloned()
            .collect();
        let buys = self.split_oversize(buys, v.ccl);
        let already = self.orders_today_outside(batch_id)?;
        let checked: Vec<PlannedOrder> = buys.iter().map(|(o, _)| o.clone()).collect();
        check_orders(
            &checked,
            round_money(self.cfg.risk.max_order_usd * v.ccl),
            max_orders_per_day,
            already,
            false,
        )?;
        Ok((buys, v.cash_ars))
    }

    /// Splits every order whose estimated value exceeds `max_order_usd` (at `ccl`) into tranches
    /// of at most that value, each a whole number of lots. Returns the orders with their tranche
    /// index (0 for an order that was not split). A single lot above the limit is left alone and
    /// the order check blocks it.
    fn split_oversize(&self, orders: Vec<PlannedOrder>, ccl: Decimal) -> Vec<(PlannedOrder, u32)> {
        let max_value = round_money(self.cfg.risk.max_order_usd * ccl);
        let mut out = Vec::with_capacity(orders.len());
        for order in orders {
            if order.est_value <= max_value || order.exec_price <= Decimal::ZERO {
                out.push((order, 0));
                continue;
            }
            let lot = self
                .universe
                .get(&order.asset)
                .and_then(|a| a.exec.as_ref())
                .map_or(1, |e| e.lot_size);
            let per_tranche = floor_to_lot(max_value / order.exec_price, lot);
            if per_tranche <= Decimal::ZERO {
                out.push((order, 0));
                continue;
            }
            let mut left = order.qty;
            let mut index = 0_u32;
            while left > Decimal::ZERO {
                let qty = per_tranche.min(left);
                let value = round_money(qty * order.exec_price);
                out.push((
                    PlannedOrder {
                        qty,
                        est_value: value,
                        est_fees: self.cfg.fees.total(value),
                        ..order.clone()
                    },
                    index,
                ));
                index += 1;
                left -= qty;
            }
        }
        out
    }

    /// Orders created today (ART) in this mode that do not belong to `batch_id`.
    fn orders_today_outside(&self, batch_id: Uuid) -> anyhow::Result<u32> {
        let (from, to) = art_day_bounds(self.today());
        let mode = self.exec_mode();
        let orders = self.db(|s| s.orders_between(mode, from, to))?;
        let n = orders.iter().filter(|o| o.batch_id != batch_id).count();
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Stores the order, sends it and records the outcome. An order that already exists in the
    /// store is never sent again.
    async fn send_order(
        &self,
        order: &PlannedOrder,
        tranche: u32,
        reason: OrderReason,
        batch_id: Uuid,
        emergency: bool,
    ) -> anyhow::Result<SendOutcome> {
        let exec = self
            .universe
            .get(&order.asset)
            .and_then(|a| a.exec.clone())
            .ok_or_else(|| anyhow!("{} no es operable", order.asset))?;
        let id = order_id_for_tranche(batch_id, &order.asset, order.side, tranche);
        let now = self.now();
        // Emergency sells: an aggressive limit (below the last price by the configured discount)
        // when a fresh quote is known, otherwise at market; the priority is getting out.
        let order_type = if emergency && order.ref_price <= Decimal::ZERO {
            OrderType::Market
        } else if emergency {
            OrderType::Limit
        } else {
            self.cfg.execution.order_type
        };
        let limit_price = match order_type {
            OrderType::Market => None,
            OrderType::Limit if emergency => {
                let tick = self.cfg.execution.price_tick;
                let discount = self.cfg.execution.emergency_limit_discount;
                let p = floor_to_tick(order.ref_price * (Decimal::ONE - discount), tick);
                Some(if p > Decimal::ZERO { p } else { tick })
            }
            OrderType::Limit => Some(self.limit_price_for(order.side, order.ref_price)),
        };
        let mut rec = OrderRecord {
            id,
            batch_id,
            created_at: now,
            updated_at: now,
            mode: self.exec_mode(),
            reason,
            asset: order.asset.clone(),
            symbol: exec.symbol.clone(),
            market: exec.market,
            side: order.side,
            qty: order.qty,
            limit_price,
            currency: exec.currency,
            status: OrderStatus::PendingSend,
            broker_order_id: None,
            filled_qty: Decimal::ZERO,
            avg_fill_price: None,
            fees: Decimal::ZERO,
            fees_estimated: false,
            error: None,
        };
        let inserted = self.db(|s| s.insert_order(&rec))?;
        if !inserted {
            // Already stored (retry of the same batch): reuse it, never place again.
            let existing = self
                .db(|s| s.get_order(id))?
                .ok_or_else(|| anyhow!("la orden {id} existe pero no se pudo leer"))?;
            let halt = (existing.status == OrderStatus::Failed
                && existing
                    .error
                    .as_deref()
                    .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME)))
            .then(|| {
                format!(
                    "La orden {} de {} quedó con resultado desconocido en un intento anterior: verificá en IOL y después usá `moon orders resolve {}`.",
                    super::text::short_id(existing.id),
                    existing.symbol,
                    super::text::short_id(existing.id)
                )
            });
            return Ok(SendOutcome { id, halt });
        }

        let ticket = OrderTicket {
            idempotency_key: id,
            symbol: exec.symbol.clone(),
            market: exec.market,
            side: order.side,
            qty: order.qty,
            order_type,
            limit_price,
            due_date: art_date(now),
            settlement_term: self.cfg.execution.settlement_term,
        };
        let result = self.deps.broker.place(&ticket).await;
        rec.updated_at = self.now();
        let mut halt = None;
        match result {
            Err(e) => {
                rec.status = OrderStatus::Failed;
                rec.error = Some(format!("{UNKNOWN_OUTCOME} {e}"));
                halt = Some(format!(
                    "La orden {} de {} {} tuvo un resultado desconocido ({e}): verificá en IOL antes de operar y después usá `moon orders resolve {}`. No se reenvía.",
                    super::text::short_id(id),
                    super::text::side_es(order.side),
                    exec.symbol,
                    super::text::short_id(id)
                ));
            }
            Ok(res) => {
                rec.broker_order_id = Some(res.broker_order_id).filter(|id| !id.is_empty());
                if !res.accepted {
                    rec.status = OrderStatus::Rejected;
                    rec.error = Some(if res.messages.is_empty() {
                        "rechazada por el broker".to_string()
                    } else {
                        res.messages.join("; ")
                    });
                } else if let Some(progress) = res.fill {
                    self.apply_progress(&mut rec, &progress);
                } else {
                    rec.status = OrderStatus::Sent;
                }
            }
        }
        self.db(|s| s.update_order(&rec))?;
        Ok(SendOutcome { id, halt })
    }

    /// Copies a broker progress report into the record, estimating fees when the broker gives none.
    pub(crate) fn apply_progress(&self, rec: &mut OrderRecord, p: &OrderProgress) {
        rec.status = p.status;
        rec.filled_qty = p.filled_qty;
        rec.avg_fill_price = p.avg_price;
        rec.updated_at = self.now();
        match (p.fees, p.avg_price) {
            (Some(f), _) => {
                rec.fees = f;
                rec.fees_estimated = false;
            }
            (None, Some(avg)) if p.filled_qty > Decimal::ZERO => {
                rec.fees = self.cfg.fees.total(round_money(p.filled_qty * avg));
                rec.fees_estimated = true;
            }
            _ => {}
        }
    }

    /// Refreshes one order from the broker and stores the result.
    pub(crate) async fn refresh_order(&self, rec: &mut OrderRecord) -> anyhow::Result<()> {
        let Some(broker_id) = rec.broker_order_id.clone() else {
            return Ok(());
        };
        let progress = self
            .deps
            .broker
            .refresh(&broker_id, art_date(rec.created_at))
            .await?;
        self.apply_progress(rec, &progress);
        self.db(|s| s.update_order(rec))?;
        Ok(())
    }

    /// Polls the orders until they are terminal or `fill_timeout_minutes` elapsed (real time).
    async fn wait_for_fills(&self, ids: &[Uuid]) {
        let timeout = Duration::from_secs(u64::from(self.cfg.execution.fill_timeout_minutes) * 60);
        let poll = Duration::from_secs(u64::from(self.cfg.execution.poll_interval_seconds))
            .max(Duration::from_secs(1));
        let start = Instant::now();
        loop {
            let mut pending = Vec::new();
            for id in ids {
                if let Ok(Some(o)) = self.db(|s| s.get_order(*id))
                    && !o.status.is_terminal()
                    && o.broker_order_id.is_some()
                {
                    pending.push(o);
                }
            }
            if pending.is_empty() || start.elapsed() >= timeout {
                return;
            }
            tokio::time::sleep(poll).await;
            for mut o in pending {
                if let Err(e) = self.refresh_order(&mut o).await {
                    tracing::warn!("no se pudo actualizar la orden {}: {e}", o.id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn tick_rounding() {
        assert_eq!(ceil_to_tick(dec!(100.011), dec!(0.01)), dec!(100.02));
        assert_eq!(ceil_to_tick(dec!(100.01), dec!(0.01)), dec!(100.01));
        assert_eq!(floor_to_tick(dec!(100.019), dec!(0.01)), dec!(100.01));
        assert_eq!(floor_to_tick(dec!(7.3), dec!(0.5)), dec!(7.0));
        assert_eq!(ceil_to_tick(dec!(7.3), dec!(0.5)), dec!(7.5));
    }
}
