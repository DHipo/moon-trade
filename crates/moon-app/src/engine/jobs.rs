//! Scheduled and interactive jobs: monthly rebalance, approvals, risk check, daily close and the
//! paper account commands. Every job takes the trading lock, so they never interleave.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use moon_broker::ExternalFlow;
use moon_core::asset::AssetId;
use moon_core::money::{floor_to_lot, round_money};
use moon_core::nav::NavTracker;
use moon_core::rebalance::{PlannedOrder, RebalancePlan};
use moon_core::risk::{DrawdownSignal, DrawdownState};
use moon_core::risk_index::RiskIndex;
use moon_core::time::{ART, art_datetime};
use moon_core::types::{DataSource, ExecMode, OrderReason, OrderStatus, Side};
use moon_core::weights::TargetWeights;
use moon_store::{
    FlowKind, FlowRecord, OrderRecord, PaperAccount, Proposal, ProposalKind, ProposalStatus,
    Snapshot,
};
use moon_yahoo::YahooClient;
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

use super::reconcile::reconcile_positions;
use super::text::{execution_summary, proposal_text};
use super::{
    Engine, ExecutionReport, MonthlyOutcome, ORDER_BLOCKED, RiskOutcome, STATE_DRAWDOWN,
    STATE_EMERGENCY_REFUGE, STATE_EMERGENCY_UNKNOWN, STATE_FLOWS_CHECKED, STATE_NAV,
    STATE_PAPER_INIT, STATE_RISK_INDEX, UNKNOWN_OUTCOME, Valuation, art_day_bounds,
    previous_trading_day,
};
use crate::config::TradingMode;

const PAPER_ONLY: &str = "Solo disponible en modo paper.";

impl Engine {
    // --- monthly rebalance ---

    pub async fn run_monthly(&self, today: NaiveDate) -> anyhow::Result<MonthlyOutcome> {
        let _guard = self.trading_lock.lock().await;
        if self.is_paused()? {
            self.deps
                .notifier
                .notify("⏸️ El bot está en pausa: se omite el rebalanceo mensual.")
                .await;
            return Ok(MonthlyOutcome::NoChanges);
        }
        let as_of = previous_trading_day(today, self.holidays());
        let decision = self.decide(as_of)?;
        let now = self.now();
        self.db(|s| {
            s.log_decision(
                now,
                "monthly",
                &json!({"as_of": as_of, "decision": &decision}),
            )
        })?;

        let v = self.valuation().await?;
        self.notify_unpriced(&v).await;
        // The risk index is chained at every observation, so the monthly decision sees the
        // current level; the breaker is only re-armed once the monthly was executed or proposed.
        let mut index = self.load_risk_index()?;
        self.observe_risk(&mut index, &v)?;
        self.save_risk_index(&index)?;
        let mut drawdown = self.load_drawdown(index.level)?;

        let targets = decision.final_weights.clone();
        let plan = self.plan(&targets, &v, false)?;
        if plan.is_empty() {
            self.finish_monthly_decision(&mut drawdown, index.level)?;
            self.deps
                .notifier
                .notify(&format!(
                    "Rebalanceo mensual (señales al {as_of}): sin cambios, la cartera ya está en línea con el objetivo."
                ))
                .await;
            return Ok(MonthlyOutcome::NoChanges);
        }

        match self.cfg.mode {
            TradingMode::Paper | TradingMode::LiveAuto => {
                let report = self
                    .execute(&plan, &targets, OrderReason::Monthly, Uuid::new_v4(), false)
                    .await?;
                self.finish_monthly_decision(&mut drawdown, index.level)?;
                Ok(MonthlyOutcome::Executed(report))
            }
            TradingMode::LiveConfirm => {
                let expires_at = self.proposal_expiry(now);
                let proposal = Proposal {
                    id: Uuid::new_v4(),
                    created_at: now,
                    expires_at,
                    kind: ProposalKind::Monthly,
                    status: ProposalStatus::Pending,
                    decided_at: None,
                    payload: json!({"decision": &decision, "targets": &targets, "plan": &plan}),
                    telegram_message_id: None,
                };
                self.db(|s| s.insert_proposal(&proposal))?;
                self.finish_monthly_decision(&mut drawdown, index.level)?;
                let expires_txt = art_datetime(expires_at).format("%H:%M").to_string();
                let text = proposal_text(as_of, &targets, &plan, &expires_txt);
                match self
                    .deps
                    .notifier
                    .request_approval(proposal.id, &text)
                    .await
                {
                    Ok(Some(message_id)) => {
                        self.db(|s| s.set_proposal_message(proposal.id, message_id))?;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("no se pudo enviar la propuesta para aprobar: {e}");
                        self.deps
                            .notifier
                            .notify(&format!(
                                "⚠️ No se pudo enviar la propuesta {} con botones: {e}\n{text}",
                                proposal.id
                            ))
                            .await;
                    }
                }
                Ok(MonthlyOutcome::ProposalSent(proposal.id))
            }
        }
    }

    /// Marks the monthly decision as taken: the first one after a trip clears the breaker and
    /// re-bases its peak at the current risk level; otherwise the peak is left alone.
    fn finish_monthly_decision(
        &self,
        drawdown: &mut DrawdownState,
        level: f64,
    ) -> anyhow::Result<()> {
        let reset = drawdown.monthly_decision(level);
        self.save_drawdown(drawdown)?;
        if reset {
            let (refuge, unknown) = (
                self.state_key(STATE_EMERGENCY_REFUGE),
                self.state_key(STATE_EMERGENCY_UNKNOWN),
            );
            self.db(|s| {
                s.delete_state(&refuge)?;
                s.delete_state(&unknown)
            })?;
        }
        Ok(())
    }

    /// Feeds the risk index with the current holdings and USD unit prices (cash is excluded).
    fn observe_risk(&self, index: &mut RiskIndex, v: &Valuation) -> anyhow::Result<f64> {
        let holdings: BTreeMap<String, Decimal> = v
            .positions
            .iter()
            .filter(|p| p.qty > Decimal::ZERO)
            .map(|p| (p.symbol.clone(), p.qty))
            .collect();
        let prices: BTreeMap<String, Decimal> = v
            .positions
            .iter()
            .map(|p| (p.symbol.clone(), p.price_ars / v.ccl))
            .collect();
        index
            .observe(&holdings, &prices, v.at)
            .map_err(|e| anyhow!("no se pudo actualizar el índice de riesgo: {e}"))
    }

    /// `now + approval_timeout`, capped at today's market close (ART).
    fn proposal_expiry(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let by_timeout =
            now + Duration::minutes(i64::from(self.cfg.execution.approval_timeout_minutes));
        let close_local = art_datetime(now)
            .date_naive()
            .and_time(self.cfg.schedule.market_close);
        match ART.from_local_datetime(&close_local).earliest() {
            Some(close) => by_timeout.min(close.with_timezone(&Utc)),
            None => by_timeout,
        }
    }

    // --- approvals ---

    pub async fn handle_approval(
        &self,
        proposal_id: Uuid,
        approve: bool,
    ) -> anyhow::Result<String> {
        let _guard = self.trading_lock.lock().await;
        let Some(proposal) = self.db(|s| s.get_proposal(proposal_id))? else {
            return Ok("Propuesta inexistente.".to_string());
        };
        if proposal.status != ProposalStatus::Pending {
            return Ok("Esta propuesta ya fue resuelta.".to_string());
        }
        let now = self.now();
        if now >= proposal.expires_at {
            self.db(|s| s.set_proposal_status(proposal_id, ProposalStatus::Expired, now))?;
            return Ok("La propuesta venció; se recalculará en el próximo rebalanceo.".to_string());
        }
        if !approve {
            self.db(|s| s.set_proposal_status(proposal_id, ProposalStatus::Rejected, now))?;
            return Ok("Propuesta rechazada: no se operó.".to_string());
        }
        if self.is_paused()? {
            return Ok(
                "El bot está en pausa: reanudalo con /reanudar para ejecutar la propuesta."
                    .to_string(),
            );
        }
        let dd_key = self.state_key(STATE_DRAWDOWN);
        let tripped = self
            .db(|s| s.get_state::<DrawdownState>(&dd_key))?
            .is_some_and(|d| d.tripped);
        if tripped {
            self.db(|s| s.set_proposal_status(proposal_id, ProposalStatus::Expired, now))?;
            return Ok(
                "El freno por caída se activó después de crear la propuesta: ya no es válida."
                    .to_string(),
            );
        }
        let targets: TargetWeights = serde_json::from_value(
            proposal
                .payload
                .get("targets")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )?;

        self.db(|s| s.set_proposal_status(proposal_id, ProposalStatus::Approved, now))?;
        let outcome = async {
            let v = self.valuation().await?;
            self.notify_unpriced(&v).await;
            let plan = self.plan(&targets, &v, false)?;
            if plan.is_empty() {
                return anyhow::Ok(None);
            }
            let report = self
                .execute(&plan, &targets, OrderReason::Monthly, Uuid::new_v4(), false)
                .await?;
            Ok(Some(report))
        }
        .await;
        let at = self.now();
        match outcome {
            Ok(None) => {
                self.db(|s| s.set_proposal_status(proposal_id, ProposalStatus::Executed, at))?;
                Ok(
                    "Aprobada: no hay operaciones por hacer, la cartera ya está en línea."
                        .to_string(),
                )
            }
            Ok(Some(report)) => {
                let status = if report.errors.is_empty() {
                    ProposalStatus::Executed
                } else {
                    ProposalStatus::Failed
                };
                self.db(|s| s.set_proposal_status(proposal_id, status, at))?;
                Ok(execution_summary(OrderReason::Monthly, &report))
            }
            Err(e) => {
                self.db(|s| s.set_proposal_status(proposal_id, ProposalStatus::Failed, at))?;
                Ok(format!("La ejecución de la propuesta falló: {e}"))
            }
        }
    }

    // --- risk check ---

    /// Drawdown breaker on the risk index (market return of what is held, in USD, cash excluded).
    /// Emergency exits run in every mode and even when paused. While the breaker is tripped every
    /// check retries what is still held, WITHOUT needing a valuation, so a rejected or cancelled
    /// emergency sell is never left behind.
    pub async fn run_risk_check(&self) -> anyhow::Result<RiskOutcome> {
        let _guard = self.trading_lock.lock().await;
        let mut index = self.load_risk_index()?;
        let mut drawdown = self.load_drawdown(index.level)?;

        if drawdown.tripped {
            let mut outcome = RiskOutcome {
                level: index.level,
                drawdown: drawdown.drawdown(index.level),
                tripped: false,
                emergency: None,
            };
            outcome.emergency = self.emergency_sweep(&drawdown, None).await?;
            self.refuge_buy_best_effort(&drawdown).await;
            return Ok(outcome);
        }

        let v = self.valuation().await.map_err(|e| {
            anyhow!("No se pudo valuar la cartera: el control de caída no corrió ({e})")
        })?;
        let today = self.today();
        self.observe_risk(&mut index, &v)?;
        let signal = drawdown.observe(index.level, today, self.cfg.risk.max_drawdown);
        self.save_risk_index(&index)?;
        self.save_drawdown(&drawdown)?;
        let dd = drawdown.drawdown(index.level);
        let mut outcome = RiskOutcome {
            level: index.level,
            drawdown: dd,
            tripped: signal == DrawdownSignal::Tripped,
            emergency: None,
        };
        if !drawdown.tripped {
            return Ok(outcome);
        }

        let header = format!(
            "🚨 Freno por caída: lo invertido cayó {:.1}% desde su máximo, medido en dólares (límite {:.1}%). \
             Se venden las posiciones de riesgo y se bloquea la operación hasta la próxima decisión mensual.",
            dd * 100.0,
            self.cfg.risk.max_drawdown * 100.0
        );
        outcome.emergency = self.emergency_sweep(&drawdown, Some(&header)).await?;
        self.refuge_buy_best_effort(&drawdown).await;
        Ok(outcome)
    }

    async fn refuge_buy_best_effort(&self, drawdown: &DrawdownState) {
        if let Err(e) = self.emergency_refuge_buy(drawdown).await {
            tracing::error!("compra del refugio tras la salida de emergencia: {e}");
            self.deps
                .notifier
                .notify(&format!(
                    "⚠️ No se pudo completar la compra del refugio tras el freno por caída: {e}"
                ))
                .await;
        }
    }

    /// Sells what is still held in risky assets, once per asset unless an order is already
    /// working or its outcome is unknown. Reads the broker's account directly: it never depends
    /// on quotes or the CCL being available.
    async fn emergency_sweep(
        &self,
        drawdown: &DrawdownState,
        header: Option<&str>,
    ) -> anyhow::Result<Option<ExecutionReport>> {
        let today = self.today();
        let mode = self.exec_mode();
        let refuge = self.universe.refuge().id.clone();

        if let Some(h) = header {
            self.deps.notifier.notify(h).await;
        }
        // Open normal orders (monthly, deposit, manual) must not keep buying into the fall.
        let notes = self.cancel_open_orders(true).await?;
        if !notes.is_empty() {
            self.deps
                .notifier
                .notify(&format!(
                    "🚨 Freno por caída: órdenes abiertas canceladas.\n• {}",
                    notes.join("\n• ")
                ))
                .await;
        }

        let account = self.deps.broker.account().await?;
        let since = art_day_bounds(drawdown.tripped_on.unwrap_or(today)).0;
        let until = self.now() + Duration::days(1);
        let emergency_orders: Vec<OrderRecord> = self
            .db(|s| s.orders_between(mode, since, until))?
            .into_iter()
            .filter(|o| o.reason == OrderReason::Emergency)
            .collect();

        // Every held risky symbol, with its quantity floored to the lot size.
        let mut held: Vec<(AssetId, String, Decimal)> = Vec::new();
        for asset in self.universe.tradable() {
            let Some(exec) = asset.exec.as_ref() else {
                continue;
            };
            if asset.id == refuge {
                continue;
            }
            let qty = account
                .positions
                .get(&exec.symbol)
                .copied()
                .unwrap_or(Decimal::ZERO);
            let qty = floor_to_lot(qty, exec.lot_size);
            if qty > Decimal::ZERO {
                held.push((asset.id.clone(), exec.symbol.clone(), qty));
            }
        }
        if header.is_some() && held.is_empty() {
            self.deps
                .notifier
                .notify("No hay posiciones de riesgo para vender.")
                .await;
        }

        let mut candidates: Vec<(AssetId, String, Decimal)> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        for (asset, symbol, qty) in held {
            let mine: Vec<&OrderRecord> = emergency_orders
                .iter()
                .filter(|o| o.asset == asset)
                .collect();
            if mine.iter().any(|o| !o.status.is_terminal()) {
                continue; // still working: leave it
            }
            let is_unknown = |o: &&OrderRecord| {
                o.status == OrderStatus::Failed
                    && o.error
                        .as_deref()
                        .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME))
            };
            if mine.iter().any(is_unknown) {
                unknown.push(symbol);
                continue; // never re-send an order whose outcome is unknown
            }
            candidates.push((asset, symbol, qty));
        }

        if !unknown.is_empty() {
            let key = self.state_key(STATE_EMERGENCY_UNKNOWN);
            let mut notified: BTreeMap<String, NaiveDate> =
                self.db(|s| s.get_state(&key))?.unwrap_or_default();
            let mut fresh = Vec::new();
            for sym in unknown {
                if notified.get(&sym) != Some(&today) {
                    notified.insert(sym.clone(), today);
                    fresh.push(sym);
                }
            }
            if !fresh.is_empty() {
                self.db(|s| s.set_state(&key, &notified))?;
                self.deps
                    .notifier
                    .notify(&format!(
                        "🚨 Freno por caída: la venta de emergencia de {} tiene resultado desconocido y NO se reenvía. \
                         Verificá en IOL si la orden existe, vendé a mano si hace falta y registrá el resultado con `moon orders resolve`.",
                        fresh.join(", ")
                    ))
                    .await;
            }
        }
        if candidates.is_empty() {
            return Ok(None);
        }

        let mut orders: Vec<PlannedOrder> = Vec::new();
        for (asset, symbol, qty) in &candidates {
            let market = self
                .universe
                .get(asset)
                .and_then(|a| a.exec.as_ref())
                .map(|e| e.market);
            // A fresh quote gives an aggressive limit; without one the order goes at market.
            let ref_price = match market {
                Some(m) => match self.deps.quotes.quote(symbol, m).await {
                    Ok(q) if q.price > Decimal::ZERO => q.price,
                    Ok(_) => Decimal::ZERO,
                    Err(e) => {
                        tracing::warn!(
                            "sin cotización de {symbol} para la venta de emergencia: {e}"
                        );
                        Decimal::ZERO
                    }
                },
                None => Decimal::ZERO,
            };
            let value = round_money(*qty * ref_price);
            orders.push(PlannedOrder {
                asset: asset.clone(),
                side: Side::Sell,
                qty: *qty,
                ref_price,
                exec_price: ref_price,
                est_value: value,
                est_fees: self.cfg.fees.total(value),
            });
        }
        if header.is_none() {
            let names: Vec<String> = orders.iter().map(|o| o.asset.to_string()).collect();
            self.deps
                .notifier
                .notify(&format!(
                    "🚨 Freno por caída activo: se reintenta la venta de emergencia de {}.",
                    names.join(", ")
                ))
                .await;
        }
        let plan = RebalancePlan {
            equity: Decimal::ZERO,
            orders,
            skipped: Vec::new(),
            est_cash_after: Decimal::ZERO,
        };
        let targets = TargetWeights::all_in(refuge);
        let report = self
            .execute(
                &plan,
                &targets,
                OrderReason::Emergency,
                Uuid::new_v4(),
                true,
            )
            .await?;
        Ok(Some(report))
    }

    /// With a non-CASH refuge: once nothing risky is held and no emergency order is working,
    /// buys the refuge once per trip, within the normal order limits.
    async fn emergency_refuge_buy(&self, drawdown: &DrawdownState) -> anyhow::Result<()> {
        let refuge = self.universe.refuge().id.clone();
        if refuge.is_cash() {
            return Ok(());
        }
        let marker = drawdown.tripped_on.unwrap_or_else(|| self.today());
        let refuge_key = self.state_key(STATE_EMERGENCY_REFUGE);
        if self.db(|s| s.get_state::<NaiveDate>(&refuge_key))? == Some(marker) {
            return Ok(());
        }
        let v = self.valuation().await?;
        if v.positions
            .iter()
            .any(|p| p.asset != refuge && p.qty > Decimal::ZERO)
        {
            return Ok(());
        }
        let mode = self.exec_mode();
        let working = self
            .db(|s| s.open_orders(mode))?
            .iter()
            .any(|o| o.reason == OrderReason::Emergency);
        if working {
            return Ok(());
        }
        // A refuge buy whose outcome is unknown may exist at IOL: never send another one.
        let since = art_day_bounds(marker).0;
        let until = self.now() + Duration::days(1);
        let unknown_buy = self
            .db(|s| s.orders_between(mode, since, until))?
            .iter()
            .any(|o| {
                o.reason == OrderReason::Emergency
                    && o.asset == refuge
                    && o.side == Side::Buy
                    && o.status == OrderStatus::Failed
                    && o.error
                        .as_deref()
                        .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME))
            });
        if unknown_buy {
            self.db(|s| s.set_state(&refuge_key, &marker))?;
            self.deps
                .notifier
                .notify(
                    "🚨 Freno por caída: la compra del refugio tiene resultado desconocido y NO se reenvía. \
                     Verificá en IOL si la orden existe y comprá a mano si hace falta.",
                )
                .await;
            return Ok(());
        }
        let targets = TargetWeights::all_in(refuge.clone());
        let full = self.plan(&targets, &v, true)?;
        let orders: Vec<PlannedOrder> = full
            .buys()
            .into_iter()
            .filter(|o| o.asset == refuge)
            .cloned()
            .collect();
        if orders.is_empty() {
            // Nothing to buy (already invested or no cash): done for this trip.
            return self.db(|s| s.set_state(&refuge_key, &marker));
        }
        let plan = RebalancePlan {
            equity: full.equity,
            orders,
            skipped: full.skipped,
            est_cash_after: full.est_cash_after,
        };
        // Normal (non-emergency) limits apply; if they block the buy, `execute` notifies.
        let result = self
            .execute(
                &plan,
                &targets,
                OrderReason::Emergency,
                Uuid::new_v4(),
                false,
            )
            .await;
        match result {
            // A definite failure (rejection) leaves the marker unset so the next check retries;
            // an unknown outcome is caught by the check above.
            Ok(report)
                if report.errors.is_empty()
                    && report.orders.iter().all(|o| {
                        !matches!(o.status, OrderStatus::Rejected | OrderStatus::Failed)
                    }) =>
            {
                self.db(|s| s.set_state(&refuge_key, &marker))?;
            }
            Ok(_) => {}
            // Blocked by the order limits: already notified, retrying would not help.
            Err(e) if e.to_string().starts_with(ORDER_BLOCKED) => {
                self.db(|s| s.set_state(&refuge_key, &marker))?;
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Detects deposits and withdrawals since the last check and applies them to the NAV
    /// tracker (performance reporting only: the breaker does not use it). Deduplicated by
    /// external id.
    async fn process_flows(&self, v: &Valuation) -> anyhow::Result<()> {
        let today = self.today();
        let mode = self.exec_mode();
        let checked_key = self.state_key(STATE_FLOWS_CHECKED);
        let nav_key = self.state_key(STATE_NAV);
        let from = self
            .db(|s| s.get_state::<NaiveDate>(&checked_key))?
            .unwrap_or(today - Duration::days(7))
            .min(today);
        let flows = self.deps.broker.external_flows(from, today).await?;
        let mut fresh: Vec<(ExternalFlow, Decimal)> = Vec::new();
        for f in flows {
            let record = FlowRecord {
                at: f.at,
                mode,
                kind: f.kind,
                amount_ars: f.amount_ars,
                external_id: Some(f.external_id.clone()),
                note: "detectado automáticamente".to_string(),
            };
            if self.db(|s| s.insert_flow(&record))? {
                let usd = round_money(f.amount_ars.abs() / v.ccl);
                let signed = if f.kind == FlowKind::Withdrawal {
                    -usd
                } else {
                    usd
                };
                fresh.push((f, signed));
            }
        }
        if !fresh.is_empty() {
            let total: Decimal = fresh.iter().map(|(_, s)| *s).sum();
            let mut equity_before = v.equity_usd - total;
            let mut tracker = match self.db(|s| s.get_state::<NavTracker>(&nav_key))? {
                Some(t) => t,
                None => NavTracker::start(equity_before),
            };
            for (f, signed) in &fresh {
                tracker.apply_flow(*signed, equity_before);
                equity_before += *signed;
                let (icon, word) = if f.kind == FlowKind::Withdrawal {
                    ("💸", "Extracción")
                } else {
                    ("💰", "Depósito")
                };
                let text = format!(
                    "{icon} {word} detectado: ARS {} (≈ USD {}). El rendimiento no cambia.",
                    f.amount_ars.abs().round_dp(2),
                    signed.abs()
                );
                self.deps.notifier.notify(&text).await;
            }
            self.db(|s| s.set_state(&nav_key, &tracker))?;
        }
        self.db(|s| s.set_state(&checked_key, &today))?;
        Ok(())
    }

    // --- daily close ---

    pub async fn run_daily_close(&self, today: NaiveDate) -> anyhow::Result<()> {
        let _guard = self.trading_lock.lock().await;
        let mode = self.exec_mode();
        let mut warnings: Vec<String> = Vec::new();

        // 1. Data sync (failures only warn).
        if let Some(yahoo) = &self.yahoo {
            warnings.extend(self.sync_signal_data(yahoo, today).await);
        }

        // 5. Cancel what is still open, then refresh. Done before reconciling so late fills count.
        warnings.extend(self.cancel_open_orders(false).await?);

        // 6. Expire stale proposals.
        let now = self.now();
        for p in self.db(|s| s.pending_proposals())? {
            if now >= p.expires_at {
                self.db(|s| s.set_proposal_status(p.id, ProposalStatus::Expired, now))?;
                warnings.push(format!("La propuesta {} venció sin respuesta.", p.id));
            }
        }

        // 2. Valuation.
        let v = self.valuation().await?;

        // 3. External deposits and withdrawals: a failure only warns, the close goes on.
        if let Err(e) = self.process_flows(&v).await {
            tracing::warn!("no se pudieron consultar depósitos/extracciones: {e}");
            warnings.push(format!(
                "No se pudieron consultar depósitos/extracciones ({e}); la conciliación de efectivo puede mostrar diferencias."
            ));
        }

        // 4. Reconcile against the previous snapshot plus everything that happened since then.
        let current: BTreeMap<String, Decimal> = v
            .positions
            .iter()
            .filter(|p| p.qty > Decimal::ZERO)
            .map(|p| (p.symbol.clone(), p.qty))
            .collect();
        let previous = self.db(|s| s.latest_snapshot(mode))?;
        if let Some(prev) = previous {
            let fills: Vec<OrderRecord> = self
                .db(|s| s.orders_between(mode, prev.at, now))?
                .into_iter()
                .filter(|o| o.created_at > prev.at)
                .collect();
            for d in reconcile_positions(&prev.positions, &fills, &current) {
                warnings.push(format!(
                    "Conciliación: {} esperado {} pero IOL informa {}.",
                    d.symbol, d.expected, d.actual
                ));
            }
            if let Some(prev_cash) = prev.cash_total_ars {
                let flows: Vec<FlowRecord> = self
                    .db(|s| s.flows_between(mode, prev.at, now))?
                    .into_iter()
                    .filter(|f| f.at > prev.at)
                    .collect();
                let expected = expected_cash(prev_cash, &fills, &flows);
                let diff = v.cash_total_ars - expected;
                let tolerance =
                    (v.equity_ars.abs() * Decimal::new(5, 3)).max(Decimal::new(1000, 0));
                if diff.abs() > tolerance {
                    warnings.push(format!(
                        "diferencia de caja: esperado {}, IOL {}, diferencia {} (puede ser un dividendo, un interés, una comisión no prevista o un movimiento manual).",
                        expected.round_dp(2),
                        v.cash_total_ars.round_dp(2),
                        diff.round_dp(2)
                    ));
                }
            }
        }

        // 7. Snapshot.
        let tracker = self.load_tracker(v.equity_usd)?;
        let nav = Self::nav_of(&tracker, v.equity_usd)?;
        let level = self.load_risk_index()?.level;
        let drawdown = self.load_drawdown(level)?;
        let snapshot = Snapshot {
            at: now,
            mode,
            cash_ars: v.cash_ars,
            cash_total_ars: Some(v.cash_total_ars),
            equity_ars: v.equity_ars,
            ccl: v.ccl,
            equity_usd: v.equity_usd,
            nav,
            units: tracker.units,
            drawdown: drawdown.drawdown(level),
            positions: current,
        };
        self.db(|s| s.insert_snapshot(&snapshot))?;

        // 8. Stale signal data.
        for asset in self.universe.risky() {
            let Some(symbol) = &asset.signal_symbol else {
                continue;
            };
            match self.db(|s| s.last_bar_date(symbol, DataSource::Yahoo))? {
                Some(last) if (today - last).num_days() <= 5 => {}
                Some(last) => warnings.push(format!(
                    "Datos desactualizados: la última vela de {symbol} es del {last}."
                )),
                None => warnings.push(format!("Sin datos de {symbol}.")),
            }
        }

        if !warnings.is_empty() {
            let text = format!(
                "⚠️ Avisos del cierre del {today}:\n• {}",
                warnings.join("\n• ")
            );
            self.deps.notifier.notify(&text).await;
        }
        self.deps
            .notifier
            .notify(&format!(
                "📈 Cierre del {today}: patrimonio USD {} (ARS {}), NAV {nav:.4}, caída {:.1}%.",
                v.equity_usd,
                v.equity_ars.round_dp(2),
                snapshot.drawdown * 100.0
            ))
            .await;
        Ok(())
    }

    /// Downloads new daily bars per signal symbol; the store lock is never held while waiting.
    async fn sync_signal_data(&self, yahoo: &YahooClient, today: NaiveDate) -> Vec<String> {
        let warmup = i64::try_from(self.allocator.warmup_months()).unwrap_or(0);
        let default_from = self.cfg.backtest.config.start - Duration::days(warmup * 31 + 31);
        let mut problems = Vec::new();
        for symbol in self.universe.signal_symbols() {
            let last = match self.db(|s| s.last_bar_date(&symbol, DataSource::Yahoo)) {
                Ok(l) => l,
                Err(e) => {
                    problems.push(format!("Datos de {symbol}: {e}"));
                    continue;
                }
            };
            let start = last.map_or(default_from, |d| d + Duration::days(1));
            if start > today {
                continue;
            }
            match yahoo.daily_bars(&symbol, start, today).await {
                Ok(bars) => {
                    if let Err(e) = self.db(|s| s.upsert_bars(&symbol, DataSource::Yahoo, &bars)) {
                        problems.push(format!("Datos de {symbol}: {e}"));
                    }
                }
                Err(e) => problems.push(format!("No se pudo descargar {symbol}: {e}")),
            }
        }
        problems
    }

    /// Cancels every non-terminal order (only the non-emergency ones when `only_non_emergency`),
    /// then refreshes it. Returns notes about what was cancelled or could not be.
    async fn cancel_open_orders(&self, only_non_emergency: bool) -> anyhow::Result<Vec<String>> {
        let mode = self.exec_mode();
        let mut notes = Vec::new();
        for mut order in self.db(|s| s.open_orders(mode))? {
            if only_non_emergency && order.reason == OrderReason::Emergency {
                continue;
            }
            let Some(broker_id) = order.broker_order_id.clone() else {
                order.status = OrderStatus::Failed;
                order.error = Some(format!(
                    "{UNKNOWN_OUTCOME} la orden no tiene identificador del broker"
                ));
                order.updated_at = self.now();
                self.db(|s| s.update_order(&order))?;
                notes.push(format!(
                    "La orden de {} no tiene identificador del broker: verificá en IOL.",
                    order.symbol
                ));
                continue;
            };
            let cancelled = self.deps.broker.cancel(&broker_id).await;
            if let Err(e) = &cancelled {
                notes.push(format!(
                    "No se pudo cancelar la orden de {}: {e}",
                    order.symbol
                ));
            }
            if let Err(e) = self.refresh_order(&mut order).await {
                notes.push(format!(
                    "No se pudo actualizar la orden de {}: {e}",
                    order.symbol
                ));
            }
            if cancelled.is_ok() && !order.status.is_terminal() {
                order.status = OrderStatus::Cancelled;
                order.updated_at = self.now();
                self.db(|s| s.update_order(&order))?;
            }
        }
        Ok(notes)
    }

    // --- paper account ---

    pub async fn paper_deposit(&self, amount_ars: Decimal) -> anyhow::Result<()> {
        let _guard = self.trading_lock.lock().await;
        if self.exec_mode() != ExecMode::Paper {
            bail!(PAPER_ONLY);
        }
        if amount_ars <= Decimal::ZERO {
            bail!("El monto del depósito debe ser mayor que 0.");
        }
        let v = self.valuation().await?;
        let mut tracker = self.load_tracker(v.equity_usd)?;
        let usd = round_money(amount_ars / v.ccl);
        let at = self.now();
        let record = FlowRecord {
            at,
            mode: ExecMode::Paper,
            kind: FlowKind::Deposit,
            amount_ars,
            external_id: None,
            note: "depósito paper".to_string(),
        };
        self.db(|s| {
            let mut account = s.paper_account()?;
            account.cash_ars += amount_ars;
            s.save_paper_account(&account)?;
            s.insert_flow(&record)?;
            Ok(())
        })?;
        tracker.apply_flow(usd, v.equity_usd);
        let nav_key = self.state_key(STATE_NAV);
        self.db(|s| s.set_state(&nav_key, &tracker))?;
        self.deps
            .notifier
            .notify(&format!(
                "💰 Depósito paper registrado: ARS {} (≈ USD {usd}). El rendimiento no cambia.",
                amount_ars.round_dp(2)
            ))
            .await;
        Ok(())
    }

    pub async fn paper_reset(&self) -> anyhow::Result<()> {
        let _guard = self.trading_lock.lock().await;
        if self.exec_mode() != ExecMode::Paper {
            bail!(PAPER_ONLY);
        }
        let initial = self.cfg.paper.initial_cash_ars;
        let keys = [
            self.state_key(STATE_NAV),
            self.state_key(STATE_DRAWDOWN),
            self.state_key(STATE_RISK_INDEX),
        ];
        self.db(|s| {
            s.save_paper_account(&PaperAccount {
                cash_ars: initial,
                positions: BTreeMap::new(),
            })?;
            for key in &keys {
                s.delete_state(key)?;
            }
            s.set_state(STATE_PAPER_INIT, &true)
        })?;
        self.deps
            .notifier
            .notify(&format!(
                "🔄 Cuenta paper reiniciada con ARS {}.",
                initial.round_dp(2)
            ))
            .await;
        Ok(())
    }
}

/// Cash the bot expects at the broker: the previous total plus own fills (sells add value minus
/// fees, buys subtract value plus fees) plus deposits minus withdrawals.
pub(crate) fn expected_cash(
    previous_total: Decimal,
    fills: &[OrderRecord],
    flows: &[FlowRecord],
) -> Decimal {
    let mut cash = previous_total;
    for o in fills {
        if o.filled_qty <= Decimal::ZERO {
            continue;
        }
        let Some(avg) = o.avg_fill_price else {
            continue;
        };
        let value = round_money(o.filled_qty * avg);
        match o.side {
            Side::Sell => cash += value - o.fees,
            Side::Buy => cash -= value + o.fees,
        }
    }
    for f in flows {
        match f.kind {
            FlowKind::Deposit => cash += f.amount_ars.abs(),
            FlowKind::Withdrawal => cash -= f.amount_ars.abs(),
        }
    }
    cash
}
