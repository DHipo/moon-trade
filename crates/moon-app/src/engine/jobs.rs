//! Scheduled and interactive jobs: monthly rebalance, approvals, risk check, daily close and the
//! paper account commands. Every job takes the trading lock, so they never interleave.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use moon_broker::ExternalFlow;
use moon_core::asset::{AssetId, Role};
use moon_core::money::{floor_to_lot, round_money};
use moon_core::nav::NavTracker;
use moon_core::rebalance::{PlannedOrder, RebalancePlan};
use moon_core::risk::{DrawdownSignal, DrawdownState};
use moon_core::risk_index::{AnomalyKind, RiskIndex};
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

use super::execution::EmergencyPricing;
use super::reconcile::reconcile_positions;
use super::text::{execution_summary, proposal_text};
use super::{
    Engine, ExecutionReport, MonthlyAttempt, MonthlyOutcome, ORDER_BLOCKED, RiskOutcome,
    STATE_DEPOSIT_PENDING, STATE_DRAWDOWN, STATE_EMERGENCY_REFUGE, STATE_EMERGENCY_UNKNOWN,
    STATE_FLOWS_CHECKED, STATE_MONTHLY_ATTEMPT, STATE_NAV, STATE_PAPER_INIT, STATE_RISK_ACCEPT,
    STATE_RISK_INDEX, UNKNOWN_OUTCOME, Valuation, art_day_bounds, last_day_of_previous_month,
};
use crate::commands::data::fetch_update;
use crate::config::TradingMode;

const PAPER_ONLY: &str = "Solo disponible en modo paper.";

/// Which attempt of the emergency sell escalation an asset is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SellStep {
    /// Limit just below the last price with a fresh quote, otherwise market.
    First,
    Market,
    /// Deep limit on the last known quote, fresh or not.
    DeepLimit,
}

/// Last time a refuge-buy failure was notified (per mode), to notify at most once per hour.
const STATE_REFUGE_NOTICE: &str = "emergency_refuge_notified";
/// Consecutive failing checks of an emergency order's status before the notice escalates.
const REFRESH_FAILURES_BEFORE_ESCALATION: u32 = 3;

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
        // Fresh signal data first; the decision uses the last calendar day of the previous month.
        if let Some(yahoo) = &self.yahoo {
            for problem in self.sync_signal_data(yahoo, today).await {
                tracing::warn!("{problem}");
            }
        }
        let as_of = last_day_of_previous_month(today);
        if let Some(problem) = self.stale_signal_data(as_of)? {
            self.deps
                .notifier
                .notify(&format!(
                    "⚠️ Rebalanceo mensual postergado: {problem}. Se reintenta más tarde."
                ))
                .await;
            return Ok(MonthlyOutcome::Aborted(problem));
        }
        let decision = self.decide(as_of)?;
        let now = self.now();
        self.db(|s| {
            s.log_decision(
                now,
                "monthly",
                self.exec_mode(),
                &json!({"as_of": as_of, "decision": &decision}),
            )
        })?;

        // Orders of this month that are still working (or whose outcome is unknown) are never
        // duplicated: their assets are left out of the plan.
        let (blocked, notes) = self.working_assets(today).await?;
        self.notify_blocked(&blocked, &notes).await;

        let v = self.valuation().await?;
        self.notify_unpriced(&v).await;
        // The risk index is chained at every observation, so the monthly decision sees the
        // current level; the breaker is only re-armed once the monthly was executed or proposed.
        let mut index = self.load_risk_index()?;
        self.observe_risk(&mut index, &v).await?;
        self.save_risk_index(&index)?;
        let mut drawdown = self.load_drawdown(index.level)?;

        let targets = decision.final_weights.clone();
        let mut plan = self.plan(&targets, &v, false)?;
        plan.orders.retain(|o| !blocked.contains(&o.asset));
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
                let batch_id = self.next_monthly_batch(today)?;
                let report = self
                    .execute_excluding(
                        &plan,
                        &targets,
                        OrderReason::Monthly,
                        batch_id,
                        false,
                        &blocked,
                    )
                    .await?;
                let outcome = MonthlyOutcome::Executed(report);
                if outcome.is_complete() {
                    self.finish_monthly_decision(&mut drawdown, index.level)?;
                } else {
                    self.deps
                        .notifier
                        .notify(
                            "⚠️ Ninguna orden del rebalanceo mensual fue aceptada: se reintenta en un rato.",
                        )
                        .await;
                }
                Ok(outcome)
            }
            TradingMode::LiveConfirm => {
                let proposal = self.insert_proposal(
                    ProposalKind::Monthly,
                    json!({"decision": &decision, "targets": &targets, "plan": &plan}),
                )?;
                self.finish_monthly_decision(&mut drawdown, index.level)?;
                let expires_txt = art_datetime(proposal.expires_at)
                    .format("%H:%M")
                    .to_string();
                let text =
                    proposal_text(ProposalKind::Monthly, as_of, &targets, &plan, &expires_txt);
                self.announce_proposal(&proposal, &text).await;
                Ok(MonthlyOutcome::ProposalSent(proposal.id))
            }
        }
    }

    /// Persists a pending proposal that expires per the approval timeout.
    fn insert_proposal(
        &self,
        kind: ProposalKind,
        payload: serde_json::Value,
    ) -> anyhow::Result<Proposal> {
        let now = self.now();
        let proposal = Proposal {
            id: Uuid::new_v4(),
            created_at: now,
            expires_at: self.proposal_expiry(now),
            kind,
            status: ProposalStatus::Pending,
            decided_at: None,
            payload,
            telegram_message_id: None,
        };
        self.db(|s| s.insert_proposal(&proposal))?;
        Ok(proposal)
    }

    /// Sends the proposal with approval buttons; falls back to a plain notice if that fails.
    async fn announce_proposal(&self, proposal: &Proposal, text: &str) {
        match self.deps.notifier.request_approval(proposal.id, text).await {
            Ok(Some(message_id)) => {
                if let Err(e) = self.db(|s| s.set_proposal_message(proposal.id, message_id)) {
                    tracing::error!("no se pudo guardar el mensaje de la propuesta: {e}");
                }
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
    }

    /// Every risky/refuge/canary signal symbol needs a bar dated within 4 days of `as_of`;
    /// otherwise a Spanish description of the first problem.
    fn stale_signal_data(&self, as_of: NaiveDate) -> anyhow::Result<Option<String>> {
        let mut symbols: BTreeSet<String> = BTreeSet::new();
        for asset in &self.cfg.assets {
            if matches!(asset.role, Role::Risky | Role::Refuge | Role::Canary)
                && let Some(sym) = &asset.signal_symbol
            {
                symbols.insert(sym.clone());
            }
        }
        for symbol in symbols {
            let last = self.db(|s| s.last_bar_date(&symbol, DataSource::Yahoo))?;
            match last {
                Some(d) if d >= as_of - Duration::days(4) => {}
                Some(d) => {
                    return Ok(Some(format!(
                        "los datos de {symbol} están desactualizados (última vela del {d}, se necesitan datos cercanos al {as_of})"
                    )));
                }
                None => return Ok(Some(format!("faltan datos de {symbol}"))),
            }
        }
        Ok(None)
    }

    /// Batch id of this month's monthly rebalance: `v5("<mode>:monthly:<YYYY-MM>:<attempt>")`.
    /// The attempt only moves on when the current one already created orders, so a retry after a
    /// crash reuses the ids of an interrupted run while a retry after rejections gets fresh ones.
    fn next_monthly_batch(&self, today: NaiveDate) -> anyhow::Result<Uuid> {
        let month = today.format("%Y-%m").to_string();
        let key = self.state_key(STATE_MONTHLY_ATTEMPT);
        let mut state = self
            .db(|s| s.get_state::<MonthlyAttempt>(&key))?
            .unwrap_or_default();
        if state.month != month {
            state = MonthlyAttempt {
                month: month.clone(),
                attempt: 0,
            };
        }
        let mode = self.exec_mode();
        let since = art_day_bounds(today.with_day(1).unwrap_or(today)).0;
        let until = self.now() + Duration::days(1);
        let used: BTreeSet<Uuid> = self
            .db(|s| s.orders_between(mode, since, until))?
            .into_iter()
            .map(|o| o.batch_id)
            .collect();
        let mut id = monthly_batch_id(mode, &month, state.attempt);
        while used.contains(&id) {
            state.attempt += 1;
            id = monthly_batch_id(mode, &month, state.attempt);
        }
        self.db(|s| s.set_state(&key, &state))?;
        Ok(id)
    }

    /// Refreshes this month's non-terminal orders and returns the assets that must not be
    /// traded again: those with a working order or an order with unknown outcome.
    async fn working_assets(
        &self,
        today: NaiveDate,
    ) -> anyhow::Result<(BTreeSet<AssetId>, Vec<String>)> {
        let mode = self.exec_mode();
        let since = art_day_bounds(today.with_day(1).unwrap_or(today)).0;
        let until = self.now() + Duration::days(1);
        let mut notes = Vec::new();
        let orders = self.db(|s| s.orders_between(mode, since, until))?;
        for mut order in orders.into_iter().filter(|o| !o.status.is_terminal()) {
            if let Err(e) = self.refresh_order(&mut order).await {
                notes.push(format!(
                    "No se pudo actualizar la orden de {}: {e}",
                    order.symbol
                ));
            }
        }
        let blocked = self
            .db(|s| s.orders_between(mode, since, until))?
            .into_iter()
            .filter(|o| {
                !o.status.is_terminal()
                    || (o.status == OrderStatus::Failed
                        && o.error
                            .as_deref()
                            .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME)))
            })
            .map(|o| o.asset)
            .collect();
        Ok((blocked, notes))
    }

    async fn notify_blocked(&self, blocked: &BTreeSet<AssetId>, notes: &[String]) {
        if blocked.is_empty() && notes.is_empty() {
            return;
        }
        let mut lines = Vec::new();
        if !blocked.is_empty() {
            // Name the blocking orders by their short id; those with an unknown outcome can be
            // resolved by hand.
            let mode = self.exec_mode();
            let today = self.today();
            let since = art_day_bounds(today.with_day(1).unwrap_or(today)).0;
            let until = self.now() + Duration::days(1);
            let orders = self
                .db(|s| s.orders_between(mode, since, until))
                .unwrap_or_default();
            let is_unknown = |o: &OrderRecord| {
                o.status == OrderStatus::Failed
                    && o.error
                        .as_deref()
                        .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME))
            };
            let mut names: Vec<String> = Vec::new();
            let mut any_unknown = false;
            for asset in blocked {
                let blocking: Vec<&OrderRecord> = orders
                    .iter()
                    .filter(|o| &o.asset == asset && (!o.status.is_terminal() || is_unknown(o)))
                    .collect();
                any_unknown |= blocking.iter().any(|o| is_unknown(o));
                if blocking.is_empty() {
                    names.push(asset.to_string());
                } else {
                    let ids: Vec<String> = blocking
                        .iter()
                        .map(|o| super::text::short_id(o.id))
                        .collect();
                    names.push(format!("{asset} (orden {})", ids.join(", ")));
                }
            }
            let mut text = format!(
                "Se omiten {} porque ya tienen una orden pendiente o con resultado desconocido este mes (verificá en IOL).",
                names.join(", ")
            );
            if any_unknown {
                text.push_str(
                    " Si ya verificaste una orden con resultado desconocido, registrala con `moon orders resolve <id>`.",
                );
            }
            lines.push(text);
        }
        lines.extend(notes.iter().cloned());
        self.deps
            .notifier
            .notify(&format!("⚠️ {}", lines.join("\n")))
            .await;
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
        // The monthly was executed or proposed with the cash in the account: deposits detected
        // before it must not trigger a second proposal for the same cash.
        let pending = self.state_key(STATE_DEPOSIT_PENDING);
        self.db(|s| s.delete_state(&pending))?;
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
    async fn observe_risk(&self, index: &mut RiskIndex, v: &Valuation) -> anyhow::Result<f64> {
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
        // Acceptances the owner requested (Telegram or CLI) apply to this observation.
        let accept_key = self.state_key(STATE_RISK_ACCEPT);
        let accepted: Vec<String> = self.db(|s| s.get_state(&accept_key))?.unwrap_or_default();
        for sym in &accepted {
            index.accept(sym);
        }
        let r = index
            .observe(&holdings, &prices, v.at)
            .map_err(|e| anyhow!("no se pudo actualizar el índice de riesgo: {e}"))?;
        if !accepted.is_empty() {
            self.db(|s| s.delete_state(&accept_key))?;
        }
        // Each anomaly is announced once per symbol and kind per day.
        let today = self.today();
        for a in &index.anomalies {
            let key = self.state_key(&format!("risk_anomaly:{}:{:?}", a.symbol, a.kind));
            let seen = self.db(|s| s.get_state::<NaiveDate>(&key))?;
            if seen == Some(today) {
                continue;
            }
            self.db(|s| s.set_state(&key, &today))?;
            let text = match a.kind {
                AnomalyKind::SuspectedRatioChange => format!(
                    "⚠️ Índice de riesgo: posible cambio de ratio en {sym} ({detail}). No se cuenta como caída hasta que IOL actualice la tenencia o lo aceptes con /aceptar {sym}.",
                    sym = a.symbol,
                    detail = a.detail
                ),
                AnomalyKind::CorporateAction => format!(
                    "⚠️ Índice de riesgo: {} — {}. No cuenta como caída de mercado.",
                    a.symbol, a.detail
                ),
                AnomalyKind::AcceptedByOwner => {
                    format!("⚠️ Índice de riesgo: {} — {}.", a.symbol, a.detail)
                }
            };
            self.deps.notifier.notify(&text).await;
        }
        Ok(r)
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
        let (reason, deposit) = match proposal.kind {
            ProposalKind::Monthly => (OrderReason::Monthly, false),
            ProposalKind::Deposit => (OrderReason::Deposit, true),
        };
        // Buys shown in the proposal, to detect a plan that moved a lot before approval.
        let shown_buys: Option<Decimal> = proposal
            .payload
            .get("plan")
            .and_then(|p| serde_json::from_value::<RebalancePlan>(p.clone()).ok())
            .map(|p| p.buys().iter().map(|o| o.est_value).sum());
        // The same proposal always executes under the same batch id.
        let batch_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, proposal.id.to_string().as_bytes());

        self.db(|s| s.set_proposal_status(proposal_id, ProposalStatus::Approved, now))?;
        let outcome = async {
            let (blocked, notes) = self.working_assets(self.today()).await?;
            self.notify_blocked(&blocked, &notes).await;
            let v = self.valuation().await?;
            self.notify_unpriced(&v).await;
            let mut plan = self.plan(&targets, &v, false)?;
            if deposit {
                plan.orders.retain(|o| o.side == Side::Buy);
            }
            plan.orders.retain(|o| !blocked.contains(&o.asset));
            if plan.is_empty() {
                return anyhow::Ok(Approval::NothingToDo);
            }
            let now_buys: Decimal = plan.buys().iter().map(|o| o.est_value).sum();
            if let Some(shown) = shown_buys
                && now_buys > shown * Decimal::new(11, 1)
            {
                return Ok(Approval::Moved { shown, now_buys });
            }
            let report = self
                .execute_excluding(&plan, &targets, reason, batch_id, false, &blocked)
                .await?;
            Ok(Approval::Done(report))
        }
        .await;
        let at = self.now();
        match outcome {
            Ok(Approval::NothingToDo) => {
                self.db(|s| s.set_proposal_status(proposal_id, ProposalStatus::Executed, at))?;
                Ok(
                    "Aprobada: no hay operaciones por hacer, la cartera ya está en línea."
                        .to_string(),
                )
            }
            Ok(Approval::Moved { shown, now_buys }) => {
                self.db(|s| s.set_proposal_status(proposal_id, ProposalStatus::Expired, at))?;
                Ok(format!(
                    "No se operó: con los precios y el efectivo de ahora las compras serían de ARS {} y la propuesta mostraba ARS {} (más de 10 % de diferencia). Esperá una nueva propuesta.",
                    now_buys.round_dp(2),
                    shown.round_dp(2)
                ))
            }
            Ok(Approval::Done(report)) => {
                let status = if report.errors.is_empty() {
                    ProposalStatus::Executed
                } else {
                    ProposalStatus::Failed
                };
                self.db(|s| s.set_proposal_status(proposal_id, status, at))?;
                Ok(execution_summary(reason, &report))
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
        self.observe_risk(&mut index, &v).await?;
        let signal = drawdown.observe_confirmed(
            index.level,
            today,
            self.cfg.risk.max_drawdown,
            self.cfg.risk.trip_confirmations,
        );
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
        // Wait for the sells to fill (only when there is a refuge to buy), then buy it in this check.
        match self.refuge_pending(drawdown) {
            Ok(true) => self.wait_for_emergency_fills().await,
            Ok(false) => {}
            Err(e) => tracing::warn!("no se pudo leer el estado del refugio: {e}"),
        }
        if let Err(e) = self.emergency_refuge_buy(drawdown).await {
            tracing::error!("compra del refugio tras la salida de emergencia: {e}");
            // Refuge-buy failures are notified at most once per hour (the retry happens every check).
            if !self.notice_due(STATE_REFUGE_NOTICE, Duration::hours(1)) {
                return;
            }
            self.deps
                .notifier
                .notify(&format!(
                    "⚠️ No se pudo completar la compra del refugio tras el freno por caída: {e}. Se reintenta en el próximo control."
                ))
                .await;
        }
    }

    /// True when the refuge is not CASH and has not been bought (or given up on) in this trip.
    fn refuge_pending(&self, drawdown: &DrawdownState) -> anyhow::Result<bool> {
        if self.universe.refuge().id.is_cash() {
            return Ok(false);
        }
        let marker = drawdown.tripped_on.unwrap_or_else(|| self.today());
        let key = self.state_key(STATE_EMERGENCY_REFUGE);
        let done: Option<NaiveDate> = self.db(|s| s.get_state(&key))?;
        Ok(done != Some(marker))
    }

    /// Sells what is still held in risky assets, once per asset unless an order is already
    /// working or its outcome is unknown. Reads the broker's account directly: it never depends
    /// on quotes or the CCL being available.
    ///
    /// The attempts per asset escalate: (1) a limit just below the last price, only with a fresh
    /// quote, otherwise market; (2) market; (3) after a DEFINITE rejection of a market order, a
    /// deep limit on the last known quote (fresh or not); then market and deep limit alternate on
    /// each further definite rejection.
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
                    "🚨 Freno por caída: órdenes abiertas sin cancelar.\n• {}",
                    notes.join("\n• ")
                ))
                .await;
        }

        // Refresh the emergency orders and replace the limit orders that did not fill in time.
        let notes = self.reprice_emergency_orders().await?;
        if !notes.is_empty() {
            self.deps
                .notifier
                .notify(&format!(
                    "🚨 Freno por caída: ventas de emergencia sin ejecutar.\n• {}",
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

        let deep_pct =
            (self.cfg.execution.emergency_deep_limit_discount * Decimal::ONE_HUNDRED).normalize();
        let mut candidates: Vec<(AssetId, String, Decimal, SellStep)> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        // A sell of another reason (monthly, deposit, manual) that is still alive, because IOL
        // has not confirmed its cancellation, holds the same shares: never sell them twice.
        let working_normal_sells: Vec<OrderRecord> = self
            .db(|s| s.open_orders(mode))?
            .into_iter()
            .filter(|o| o.reason != OrderReason::Emergency && o.side == Side::Sell)
            .collect();
        let mut waiting: Vec<String> = Vec::new();
        for (asset, symbol, qty) in held {
            if let Some(o) = working_normal_sells.iter().find(|o| o.asset == asset) {
                if self.notice_due(
                    &format!("emergency_waits_for_cancel_notice:{}", o.id),
                    Duration::hours(1),
                ) {
                    waiting.push(format!(
                        "la venta de emergencia de {symbol} espera: la orden {} ({}) sigue vigente porque IOL no confirmó su cancelación. No se vende dos veces lo mismo; se reintenta la cancelación en cada control.",
                        super::text::short_id(o.id),
                        super::text::side_es(o.side)
                    ));
                }
                continue;
            }
            let mine: Vec<&OrderRecord> = emergency_orders
                .iter()
                .filter(|o| o.asset == asset && o.side == Side::Sell)
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
                let ids: Vec<String> = mine
                    .iter()
                    .copied()
                    .filter(is_unknown)
                    .map(|o| super::text::short_id(o.id))
                    .collect();
                unknown.push(format!("{symbol} (orden {})", ids.join(", ")));
                continue; // never re-send an order whose outcome is unknown
            }
            // The next attempt depends on the last sell that did not fill: after a limit order
            // (cancelled or rejected) comes market; after a market order that was definitely
            // rejected comes a deep limit.
            let last = mine
                .iter()
                .rev()
                .copied()
                .find(|o| o.status != OrderStatus::Filled);
            let step = match last {
                None => SellStep::First,
                Some(o) if o.limit_price.is_some() => SellStep::Market,
                Some(o) if o.status == OrderStatus::Rejected => SellStep::DeepLimit,
                Some(_) => SellStep::Market,
            };
            if let Some(o) = last
                && o.status == OrderStatus::Rejected
                && self.notice_due(
                    &format!("emergency_reject_notice:{}", o.id),
                    Duration::days(3650),
                )
            {
                let how = match step {
                    SellStep::DeepLimit => {
                        format!("a límite {deep_pct} % por debajo del último precio conocido")
                    }
                    _ => "a mercado".to_string(),
                };
                self.deps
                    .notifier
                    .notify(&format!(
                        "🚨 Freno por caída: la venta de emergencia de {symbol} (orden {}) fue rechazada por IOL: {}. Se reintenta {how}.",
                        super::text::short_id(o.id),
                        o.error.as_deref().unwrap_or("sin detalle")
                    ))
                    .await;
            }
            candidates.push((asset, symbol, qty, step));
        }

        if !waiting.is_empty() {
            self.deps
                .notifier
                .notify(&format!(
                    "🚨 Freno por caída: ventas de emergencia en espera.\n• {}",
                    waiting.join("\n• ")
                ))
                .await;
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
                         Verificá en IOL si la orden existe, vendé a mano si hace falta y registrá el resultado con `moon orders resolve <id>` (el id va entre paréntesis).",
                        fresh.join(", ")
                    ))
                    .await;
            }
        }
        if candidates.is_empty() {
            return Ok(None);
        }

        let mut orders: Vec<PlannedOrder> = Vec::new();
        let mut pricing: BTreeMap<AssetId, EmergencyPricing> = BTreeMap::new();
        let now = self.now();
        for (asset, symbol, qty, step) in &candidates {
            let market = self
                .universe
                .get(asset)
                .and_then(|a| a.exec.as_ref())
                .map(|e| e.market);
            let quote = match (step, market) {
                (SellStep::Market, _) | (_, None) => None,
                (_, Some(m)) => match self.deps.quotes.quote(symbol, m).await {
                    Ok(q) if q.price > Decimal::ZERO => Some(q),
                    Ok(_) => None,
                    Err(e) => {
                        tracing::warn!(
                            "sin cotización de {symbol} para la venta de emergencia: {e}"
                        );
                        None
                    }
                },
            };
            let (choice, ref_price) = match (step, quote) {
                // A fresh quote gives an aggressive limit; otherwise the order goes at market.
                (SellStep::First, Some(q)) if q.is_fresh(now) => (
                    EmergencyPricing::Limit {
                        discount: self.cfg.execution.emergency_limit_discount,
                    },
                    q.price,
                ),
                (SellStep::First, Some(q)) => {
                    tracing::warn!(
                        "la cotización de {symbol} no es fresca ({}): la venta de emergencia va a mercado",
                        q.at
                    );
                    (EmergencyPricing::Market, Decimal::ZERO)
                }
                // The deep limit trusts the last known quote even when it is stale.
                (SellStep::DeepLimit, Some(q)) => (
                    EmergencyPricing::Limit {
                        discount: self.cfg.execution.emergency_deep_limit_discount,
                    },
                    q.price,
                ),
                _ => (EmergencyPricing::Market, Decimal::ZERO),
            };
            pricing.insert(asset.clone(), choice);
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
            // At most once per hour per symbol (the retry runs at every check).
            let names: Vec<String> = candidates
                .iter()
                .filter(|(_, symbol, _, _)| {
                    self.notice_due(
                        &format!("emergency_retry_notice:{symbol}"),
                        Duration::hours(1),
                    )
                })
                .map(|(asset, _, _, _)| asset.to_string())
                .collect();
            if !names.is_empty() {
                self.deps
                    .notifier
                    .notify(&format!(
                        "🚨 Freno por caída activo: se reintenta la venta de emergencia de {}.",
                        names.join(", ")
                    ))
                    .await;
            }
        }
        let plan = RebalancePlan {
            equity: Decimal::ZERO,
            orders,
            skipped: Vec::new(),
            est_cash_after: Decimal::ZERO,
        };
        let targets = TargetWeights::all_in(refuge);
        let report = self
            .execute_emergency(&plan, &targets, Uuid::new_v4(), &pricing)
            .await?;
        Ok(Some(report))
    }

    /// Refreshes every non-terminal emergency order. A still-unfilled emergency LIMIT sell older
    /// than `emergency_reprice_minutes` is cancelled; only when the refreshed state CONFIRMS it
    /// is over (cancelled or rejected) does the sweep re-send what is still held. If the cancel
    /// is not confirmed the order is left as it is (it may still fill) and retried at the next
    /// check, so two sells are never alive at once. Orders without a broker id are left alone.
    /// Returns notes for the owner.
    async fn reprice_emergency_orders(&self) -> anyhow::Result<Vec<String>> {
        let mode = self.exec_mode();
        let window = Duration::minutes(i64::from(self.cfg.execution.emergency_reprice_minutes));
        let mut notes = Vec::new();
        let open: Vec<OrderRecord> = self
            .db(|s| s.open_orders(mode))?
            .into_iter()
            .filter(|o| o.reason == OrderReason::Emergency)
            .collect();
        for mut order in open {
            let Some(broker_id) = order.broker_order_id.clone() else {
                continue;
            };
            let id = super::text::short_id(order.id);
            let fail_key = self.state_key(&format!("emergency_refresh_failures:{}", order.id));
            if let Err(e) = self.refresh_emergency_order(&mut order).await {
                tracing::warn!(
                    "no se pudo actualizar la orden de emergencia {}: {e}",
                    order.id
                );
                // Without a status nothing is cancelled or re-sent: the owner has to know.
                let failures = self.db(|s| s.get_state::<u32>(&fail_key))?.unwrap_or(0) + 1;
                self.db(|s| s.set_state(&fail_key, &failures))?;
                if failures >= REFRESH_FAILURES_BEFORE_ESCALATION {
                    if self.notice_due(
                        &format!("emergency_refresh_escalated_notice:{}", order.id),
                        Duration::hours(1),
                    ) {
                        notes.push(format!(
                            "no se pudo consultar el estado de la orden {id} en IOL en {failures} controles seguidos (venta de {}): verificá en IOL y considerá vender a mano (registralo con `moon orders resolve {id}`). No se reenvía.",
                            order.symbol
                        ));
                    }
                } else if self.notice_due(
                    &format!("emergency_refresh_notice:{}", order.id),
                    Duration::hours(1),
                ) {
                    notes.push(format!(
                        "no se pudo consultar el estado de la orden {id} en IOL; se reintenta en el próximo control (venta de {}).",
                        order.symbol
                    ));
                }
                continue;
            }
            if self.db(|s| s.get_state::<u32>(&fail_key))?.is_some() {
                self.db(|s| s.delete_state(&fail_key))?;
            }
            let aged = !order.status.is_terminal()
                && order.side == Side::Sell
                && self.now() - order.created_at >= window;
            if aged
                && order.limit_price.is_none()
                && self.notice_due(
                    &format!("emergency_market_pending_notice:{}", order.id),
                    Duration::hours(1),
                )
            {
                // A market order is never re-sent while it works: only tell the owner.
                notes.push(format!(
                    "La venta de {} a mercado (orden {id}) sigue sin ejecutarse tras {} minutos. No se reenvía mientras siga vigente: verificá en IOL y, si hace falta, vendé a mano y registralo con `moon orders resolve {id}`.",
                    order.symbol, self.cfg.execution.emergency_reprice_minutes
                ));
            }
            let stale = aged && order.limit_price.is_some();
            if !stale {
                continue;
            }
            let cancel = self.deps.broker.cancel(&broker_id).await;
            // Act only on the refreshed state, whatever the cancel call said.
            let refreshed = self.refresh_emergency_order(&mut order).await;
            if let Err(e) = &refreshed {
                tracing::warn!(
                    "no se pudo actualizar la orden de emergencia {} tras cancelarla: {e}",
                    order.id
                );
            }
            if refreshed.is_ok()
                && matches!(order.status, OrderStatus::Cancelled | OrderStatus::Rejected)
            {
                notes.push(format!(
                    "La venta de {} a límite (orden {id}) no se ejecutó en {} minutos: IOL confirmó la cancelación y se reenvía a mercado lo que queda.",
                    order.symbol, self.cfg.execution.emergency_reprice_minutes
                ));
                continue;
            }
            if refreshed.is_ok() && order.status.is_terminal() {
                continue; // filled meanwhile: recorded and notified, nothing to re-send
            }
            // Not confirmed: never re-send, retry at the next check.
            match cancel {
                Err(e) => {
                    if self.notice_due(
                        &format!("emergency_cancel_notice:{}", order.symbol),
                        Duration::hours(1),
                    ) {
                        notes.push(format!(
                            "No se pudo cancelar la venta de {} a límite (orden {id}): {e}. Sigue vigente y no se reenvía; se reintenta en el próximo control.",
                            order.symbol
                        ));
                    }
                }
                Ok(()) => {
                    if self.notice_due(
                        &format!("emergency_cancel_unconfirmed_notice:{}", order.id),
                        Duration::hours(1),
                    ) {
                        notes.push(format!(
                            "La cancelación de la venta de {} a límite (orden {id}) no está confirmada por IOL: sigue vigente y no se reenvía; se reintenta en el próximo control.",
                            order.symbol
                        ));
                    }
                }
            }
        }
        Ok(notes)
    }

    /// Polls the working emergency SELL orders until none is left or `emergency_fill_wait_seconds`
    /// pass, so the refuge can be bought in the same risk check.
    async fn wait_for_emergency_fills(&self) {
        let wait = std::time::Duration::from_secs(u64::from(
            self.cfg.execution.emergency_fill_wait_seconds,
        ));
        let poll =
            std::time::Duration::from_secs(u64::from(self.cfg.execution.poll_interval_seconds))
                .max(std::time::Duration::from_millis(50));
        let start = std::time::Instant::now();
        let mode = self.exec_mode();
        loop {
            let working: Vec<OrderRecord> = match self.db(|s| s.open_orders(mode)) {
                Ok(v) => v
                    .into_iter()
                    .filter(|o| {
                        o.reason == OrderReason::Emergency
                            && o.side == Side::Sell
                            && o.broker_order_id.is_some()
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!("no se pudieron leer las órdenes de emergencia: {e}");
                    return;
                }
            };
            let elapsed = start.elapsed();
            if working.is_empty() || elapsed >= wait {
                return;
            }
            tokio::time::sleep(poll.min(wait - elapsed)).await;
            for mut o in working {
                if let Err(e) = self.refresh_emergency_order(&mut o).await {
                    tracing::warn!("no se pudo actualizar la orden {}: {e}", o.id);
                }
            }
        }
    }

    /// With a non-CASH refuge: once nothing risky is held and no emergency order is working, buys
    /// the refuge within the normal order limits. The trip is marked "refuge done" only when the
    /// refuge is HELD (a buy filled, or nothing left to buy); a buy that is merely sent, or was
    /// cancelled or rejected, is retried at the next check, the next trading day included. When
    /// the sells filled but their cash is not credited yet, it waits and retries.
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
        // Without a refuge quote the plan would silently fall back to CASH: that is an error to
        // notify and retry, never "done".
        if v.unpriced.contains(&refuge) {
            bail!(
                "no hay cotización del refugio {refuge}: no se puede comprar y la plata sigue en pesos"
            );
        }
        let risky: Vec<String> = v
            .positions
            .iter()
            .filter(|p| p.asset != refuge && p.qty > Decimal::ZERO)
            .map(|p| p.asset.to_string())
            .collect();
        if !risky.is_empty() {
            if self.notice_due("emergency_refuge_blocked_notice", Duration::days(1)) {
                self.deps
                    .notifier
                    .notify(&format!(
                        "🚨 Freno por caída: la compra del refugio espera: todavía queda en cartera {} (por ejemplo, una venta con resultado desconocido o sin ejecutar). Se reintenta en cada control.",
                        risky.join(", ")
                    ))
                    .await;
            }
            return Ok(());
        }
        let mode = self.exec_mode();
        let working = self
            .db(|s| s.open_orders(mode))?
            .iter()
            .any(|o| o.reason == OrderReason::Emergency);
        if working {
            return Ok(()); // refreshed by the sweep at every check
        }
        let since = art_day_bounds(marker).0;
        let until = self.now() + Duration::days(1);
        let trip_orders: Vec<OrderRecord> = self
            .db(|s| s.orders_between(mode, since, until))?
            .into_iter()
            .filter(|o| o.reason == OrderReason::Emergency)
            .collect();
        // A refuge buy whose outcome is unknown may exist at IOL: never send another one.
        let unknown_ids: Vec<String> = trip_orders
            .iter()
            .filter(|o| {
                o.asset == refuge
                    && o.side == Side::Buy
                    && o.status == OrderStatus::Failed
                    && o.error
                        .as_deref()
                        .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME))
            })
            .map(|o| super::text::short_id(o.id))
            .collect();
        if !unknown_ids.is_empty() {
            self.db(|s| s.set_state(&refuge_key, &marker))?;
            self.deps
                .notifier
                .notify(&format!(
                    "🚨 Freno por caída: la compra del refugio tiene resultado desconocido (orden {}) y NO se reenvía. \
                     Verificá en IOL si la orden existe, comprá a mano si hace falta y registrá el resultado con `moon orders resolve {}`.",
                    unknown_ids.join(", "),
                    unknown_ids[0]
                ))
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
            // Investable cash is below the minimum trade. Done only if the sells' cash has
            // been credited; otherwise wait and retry.
            if self
                .refuge_cash_settled(&v, &refuge, &trip_orders, true)
                .await
            {
                self.db(|s| s.set_state(&refuge_key, &marker))?;
            }
            return Ok(());
        }
        let plan = RebalancePlan {
            equity: full.equity,
            orders,
            skipped: full.skipped,
            est_cash_after: full.est_cash_after,
        };
        // The order size limit applies (tranches); the daily order limit does not. If the size
        // limit blocks the buy, `execute` notifies.
        let result = self
            .execute_refuge_buy(&plan, &targets, Uuid::new_v4())
            .await;
        match result {
            // After a fully filled buy, "done" only if no investable cash is left and every
            // peso of the sells is credited; otherwise later checks buy with what is credited
            // next. Sent, cancelled or rejected buys leave the marker unset too.
            Ok(report) => {
                let filled = report.errors.is_empty()
                    && !report.orders.is_empty()
                    && report
                        .orders
                        .iter()
                        .all(|o| o.status == OrderStatus::Filled);
                if filled && let Ok(v2) = self.valuation().await {
                    let left = self
                        .plan(&targets, &v2, true)?
                        .buys()
                        .into_iter()
                        .any(|o| o.asset == refuge);
                    if !left
                        && self
                            .refuge_cash_settled(&v2, &refuge, &trip_orders, false)
                            .await
                    {
                        self.db(|s| s.set_state(&refuge_key, &marker))?;
                    }
                }
            }
            // Blocked by the order limits: already notified, retrying would not help.
            Err(e) if e.to_string().starts_with(ORDER_BLOCKED) => {
                self.db(|s| s.set_state(&refuge_key, &marker))?;
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// True when the cash of the trip's sells is (almost) all credited: cash plus refuge value
    /// reach the sold amount, within the minimum trade or 2 %. Otherwise it may tell the owner
    /// (once per day) that the refuge waits for the credit.
    async fn refuge_cash_settled(
        &self,
        v: &Valuation,
        refuge: &AssetId,
        trip_orders: &[OrderRecord],
        notify: bool,
    ) -> bool {
        let sold: Decimal = trip_orders
            .iter()
            .filter(|o| o.side == Side::Sell && o.filled_qty > Decimal::ZERO)
            .filter_map(|o| o.avg_fill_price.map(|p| round_money(o.filled_qty * p)))
            .sum();
        let refuge_value: Decimal = v
            .positions
            .iter()
            .filter(|p| p.asset == *refuge)
            .map(|p| p.value_ars)
            .sum();
        let credited = v.cash_ars + refuge_value;
        let tolerance =
            round_money(self.cfg.risk.min_trade_usd * v.ccl).max(sold * Decimal::new(2, 2));
        if sold <= Decimal::ZERO || credited + tolerance >= sold {
            return true;
        }
        if notify && self.notice_due("emergency_refuge_waiting_notice", Duration::days(1)) {
            self.deps
                .notifier
                .notify(&format!(
                    "🚨 Freno por caída: esperando acreditación del efectivo de las ventas (vendido ARS {}, disponible ARS {}) para comprar el refugio {refuge}. Se reintenta en el próximo control.",
                    sold.round_dp(2),
                    v.cash_ars.round_dp(2)
                ))
                .await;
        }
        false
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
        let mut new_deposits: Vec<String> = Vec::new();
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
                if f.kind == FlowKind::Deposit {
                    new_deposits.push(f.external_id.clone());
                }
                fresh.push((f, signed));
            }
        }
        self.add_pending_deposits(new_deposits)?;
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

    // --- deposit-triggered buys ---

    fn add_pending_deposits(&self, ids: Vec<String>) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let key = self.state_key(STATE_DEPOSIT_PENDING);
        let mut pending: Vec<String> = self.db(|s| s.get_state(&key))?.unwrap_or_default();
        pending.extend(ids);
        self.db(|s| s.set_state(&key, &pending))
    }

    fn is_tripped(&self) -> anyhow::Result<bool> {
        let key = self.state_key(STATE_DRAWDOWN);
        Ok(self
            .db(|s| s.get_state::<DrawdownState>(&key))?
            .is_some_and(|d| d.tripped))
    }

    /// Target weights of the latest monthly decision of the current mode, if there is one.
    fn last_monthly_targets(&self) -> anyhow::Result<Option<TargetWeights>> {
        let mode = self.exec_mode();
        let decisions = self.db(|s| s.recent_decisions(50, mode))?;
        Ok(decisions
            .into_iter()
            .find(|d| d.kind == "monthly")
            .and_then(|d| {
                d.payload
                    .get("decision")
                    .and_then(|x| x.get("final_weights"))
                    .and_then(|w| serde_json::from_value::<TargetWeights>(w.clone()).ok())
            }))
    }

    /// Light check for the scheduler's risk tick: detects deposits and invests what is pending.
    /// Skipped while paused or while the breaker is tripped (the deposit stays pending). A
    /// failure here never affects the risk check, which runs separately.
    pub async fn check_deposits(&self) -> anyhow::Result<()> {
        let _guard = self.trading_lock.lock().await;
        if self.is_paused()? || self.is_tripped()? {
            return Ok(());
        }
        let v = self.valuation().await?;
        self.process_flows(&v).await?;
        self.invest_pending_deposits(&v).await
    }

    /// Deposits that were detected but not invested yet are bought toward the last monthly
    /// decision's weights, BUYS only (reason `Deposit`). Paper and live_auto execute; live_confirm
    /// sends a proposal. The caller holds the trading lock and has checked pause and breaker.
    async fn invest_pending_deposits(&self, v: &Valuation) -> anyhow::Result<()> {
        let key = self.state_key(STATE_DEPOSIT_PENDING);
        let pending: Vec<String> = self.db(|s| s.get_state(&key))?.unwrap_or_default();
        if pending.is_empty() {
            return Ok(());
        }
        let Some(targets) = self.last_monthly_targets()? else {
            // No monthly decision yet: the first monthly rebalance invests the deposit.
            return Ok(());
        };
        let (blocked, notes) = self.working_assets(self.today()).await?;
        self.notify_blocked(&blocked, &notes).await;
        self.notify_unpriced(v).await;
        let mut plan = self.plan(&targets, v, false)?;
        plan.orders.retain(|o| o.side == Side::Buy);
        plan.orders.retain(|o| !blocked.contains(&o.asset));
        if plan.is_empty() {
            self.db(|s| s.delete_state(&key))?;
            self.deps
                .notifier
                .notify("💰 Depósito detectado: no hay compras por hacer, la cartera ya está en línea con el objetivo.")
                .await;
            return Ok(());
        }
        let mode = self.exec_mode();
        let batch_id = Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("{}:deposit:{}", mode.as_str(), pending.join(",")).as_bytes(),
        );
        match self.cfg.mode {
            TradingMode::Paper | TradingMode::LiveAuto => {
                match self
                    .execute_excluding(
                        &plan,
                        &targets,
                        OrderReason::Deposit,
                        batch_id,
                        false,
                        &blocked,
                    )
                    .await
                {
                    Ok(_) => self.db(|s| s.delete_state(&key))?,
                    // Blocked by the order limits: already notified, retrying would not help.
                    Err(e) if e.to_string().starts_with(ORDER_BLOCKED) => {
                        self.db(|s| s.delete_state(&key))?;
                    }
                    Err(e) => return Err(e),
                }
            }
            TradingMode::LiveConfirm => {
                let proposal = self.insert_proposal(
                    ProposalKind::Deposit,
                    json!({"targets": &targets, "plan": &plan}),
                )?;
                self.db(|s| s.delete_state(&key))?;
                let expires_txt = art_datetime(proposal.expires_at)
                    .format("%H:%M")
                    .to_string();
                let text = proposal_text(
                    ProposalKind::Deposit,
                    self.today(),
                    &targets,
                    &plan,
                    &expires_txt,
                );
                self.announce_proposal(&proposal, &text).await;
            }
        }
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

    /// Downloads new daily bars per signal symbol (re-reading a 90-day overlap and the full
    /// history when it was restated); the store lock is never held while waiting.
    pub(crate) async fn sync_signal_data(
        &self,
        yahoo: &YahooClient,
        today: NaiveDate,
    ) -> Vec<String> {
        let warmup = i64::try_from(self.allocator.warmup_months()).unwrap_or(0);
        let default_from = self.cfg.backtest.config.start - Duration::days(warmup * 31 + 31);
        let now = self.now();
        let mut problems = Vec::new();
        for symbol in self.universe.signal_symbols() {
            let existing = match self.db(|s| s.load_bars(&symbol, DataSource::Yahoo)) {
                Ok(b) => b,
                Err(e) => {
                    problems.push(format!("Datos de {symbol}: {e}"));
                    continue;
                }
            };
            match fetch_update(yahoo, &symbol, &existing, default_from, today, now).await {
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

    /// Cancels every non-terminal order (only the non-emergency ones when `only_non_emergency`)
    /// and then refreshes it: an order only becomes `Cancelled` (or filled, or rejected) when the
    /// refreshed state from the broker says so. Returns notes about what could not be cancelled
    /// or confirmed (at most once per hour per order).
    async fn cancel_open_orders(&self, only_non_emergency: bool) -> anyhow::Result<Vec<String>> {
        let mode = self.exec_mode();
        let mut notes = Vec::new();
        for mut order in self.db(|s| s.open_orders(mode))? {
            if only_non_emergency && order.reason == OrderReason::Emergency {
                continue;
            }
            let id = super::text::short_id(order.id);
            let Some(broker_id) = order.broker_order_id.clone() else {
                order.status = OrderStatus::Failed;
                order.error = Some(format!(
                    "{UNKNOWN_OUTCOME} la orden no tiene identificador del broker"
                ));
                order.updated_at = self.now();
                self.db(|s| s.update_order(&order))?;
                notes.push(format!(
                    "La orden {id} de {} no tiene identificador del broker: verificá en IOL y registrá el resultado con `moon orders resolve {id}`.",
                    order.symbol
                ));
                continue;
            };
            let cancel = self.deps.broker.cancel(&broker_id).await;
            let refreshed = if order.reason == OrderReason::Emergency {
                self.refresh_emergency_order(&mut order).await
            } else {
                self.refresh_order(&mut order).await
            };
            if refreshed.is_ok() && order.status.is_terminal() {
                continue; // confirmed by the broker (cancelled, rejected or filled meanwhile)
            }
            if !self.notice_due(&format!("cancel_notice:{}", order.id), Duration::hours(1)) {
                continue;
            }
            notes.push(match (&cancel, &refreshed) {
                (_, Err(e)) => format!(
                    "No se pudo confirmar la cancelación de la orden {id} de {}: {e}. Se reintenta.",
                    order.symbol
                ),
                (Err(e), Ok(())) => format!(
                    "No se pudo cancelar la orden {id} de {}: {e}. Sigue vigente; se reintenta.",
                    order.symbol
                ),
                (Ok(()), Ok(())) => format!(
                    "La cancelación de la orden {id} de {} no está confirmada por IOL: sigue vigente; se reintenta.",
                    order.symbol
                ),
            });
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
        // The deposit is invested toward the last monthly decision (buys only), unless the bot
        // is paused or the breaker is tripped: then it stays pending.
        self.add_pending_deposits(vec![format!("paper:{}", at.to_rfc3339())])?;
        if !self.is_paused()? && !self.is_tripped()? {
            let v2 = self.valuation().await?;
            self.invest_pending_deposits(&v2).await?;
        }
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

/// What the approval of a proposal ended up doing.
enum Approval {
    NothingToDo,
    /// The buys of the recalculated plan exceed the shown ones by more than 10 %.
    Moved {
        shown: Decimal,
        now_buys: Decimal,
    },
    Done(ExecutionReport),
}

/// Batch id of the monthly rebalance: `v5("<mode>:monthly:<YYYY-MM>:<attempt>")`.
pub fn monthly_batch_id(mode: ExecMode, month: &str, attempt: u32) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("{}:monthly:{month}:{attempt}", mode.as_str()).as_bytes(),
    )
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
