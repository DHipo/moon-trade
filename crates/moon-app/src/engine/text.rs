//! Spanish texts for notifications, status and the weekly report.

use chrono::Duration;
use moon_core::money::round_money;
use moon_core::nav::NavTracker;
use moon_core::rebalance::RebalancePlan;
use moon_core::risk::DrawdownState;
use moon_core::risk_index::RiskIndex;
use moon_core::time::art_date;
use moon_core::types::{ExecMode, OrderReason, OrderStatus, Side};
use moon_core::weights::TargetWeights;
use moon_store::{FlowKind, OrderRecord, ProposalKind, Store, StoreError};
use rust_decimal::Decimal;
use uuid::Uuid;

use super::{
    Engine, ExecutionReport, STATE_DRAWDOWN, STATE_NAV, STATE_RISK_INDEX, UNKNOWN_OUTCOME,
    Valuation,
};
use crate::config::TradingMode;

pub(crate) fn side_es(side: Side) -> &'static str {
    match side {
        Side::Buy => "compra",
        Side::Sell => "venta",
    }
}

pub(crate) fn status_es(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::PendingSend => "pendiente de envío",
        OrderStatus::Sent => "enviada",
        OrderStatus::PartiallyFilled => "parcialmente ejecutada",
        OrderStatus::Filled => "ejecutada",
        OrderStatus::Cancelled => "cancelada",
        OrderStatus::Rejected => "rechazada",
        OrderStatus::Failed => "fallida",
    }
}

fn mode_es(mode: TradingMode) -> &'static str {
    match mode {
        TradingMode::Paper => "paper",
        TradingMode::LiveConfirm => "live (con confirmación)",
        TradingMode::LiveAuto => "live (automático)",
    }
}

fn ars(d: Decimal) -> String {
    format!("ARS {}", d.round_dp(2))
}

fn pct(f: f64) -> String {
    format!("{:.1}%", f * 100.0)
}

fn weight_pct(value: Decimal, equity: Decimal) -> String {
    if equity <= Decimal::ZERO {
        return "0.0%".to_string();
    }
    format!("{}%", (value / equity * Decimal::ONE_HUNDRED).round_dp(1))
}

/// Short form of an order id (first 8 hex characters of the UUID), shown in every order line.
pub fn short_id(id: Uuid) -> String {
    id.simple().to_string().chars().take(8).collect()
}

fn is_unknown_outcome(o: &OrderRecord) -> bool {
    o.status == OrderStatus::Failed
        && o.error
            .as_deref()
            .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME))
}

/// Orders that need attention: open ones and those with an unknown outcome (or only the latter
/// when `unknown_only`), oldest first.
pub(crate) fn unresolved_orders(
    store: &Store,
    mode: ExecMode,
    unknown_only: bool,
) -> Result<Vec<OrderRecord>, StoreError> {
    let mut orders = if unknown_only {
        Vec::new()
    } else {
        store.open_orders(mode)?
    };
    orders.extend(
        store
            .orders_with_status(mode, OrderStatus::Failed)?
            .into_iter()
            .filter(is_unknown_outcome),
    );
    orders.sort_by_key(|o| o.created_at);
    Ok(orders)
}

/// One line describing a stored order, led by its short id.
pub(crate) fn order_line(o: &OrderRecord) -> String {
    let mut line = format!(
        "• {} {} {}: {} — {}",
        short_id(o.id),
        side_es(o.side),
        o.symbol,
        o.qty,
        status_es(o.status)
    );
    if let Some(e) = &o.error {
        line.push_str(&format!(" [{e}]"));
    }
    line
}

/// Summary of an execution batch.
pub(crate) fn execution_summary(reason: OrderReason, report: &ExecutionReport) -> String {
    let unknown = report.orders.iter().any(|o| {
        o.error
            .as_deref()
            .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME))
    });
    let head = if unknown {
        "🚨 Ejecución con resultado desconocido"
    } else if report.errors.is_empty() {
        "✅ Ejecución completada"
    } else {
        "⚠️ Ejecución con problemas"
    };
    let mut lines = vec![format!(
        "{head} ({}): {} orden(es).",
        Engine::reason_label(reason),
        report.orders.len()
    )];
    for o in &report.orders {
        let price = o
            .avg_fill_price
            .or(o.limit_price)
            .map_or_else(|| "a mercado".to_string(), ars);
        let est = if o.fees_estimated { " (estimada)" } else { "" };
        let mut line = format!(
            "• {} {} {}: {} de {} a {} — {}; ejecutado {}; comisión {}{est}",
            short_id(o.id),
            side_es(o.side),
            o.symbol,
            o.qty,
            o.symbol,
            price,
            status_es(o.status),
            o.filled_qty,
            ars(o.fees),
        );
        if let Some(e) = &o.error {
            line.push_str(&format!(" [{e}]"));
        }
        lines.push(line);
    }
    for e in &report.errors {
        lines.push(format!("⚠️ {e}"));
    }
    lines.join("\n")
}

/// Text of a monthly proposal awaiting approval.
pub(crate) fn proposal_text(
    kind: ProposalKind,
    as_of: chrono::NaiveDate,
    targets: &TargetWeights,
    plan: &RebalancePlan,
    expires_at_art: &str,
) -> String {
    let title = match kind {
        ProposalKind::Monthly => format!("📋 Propuesta de rebalanceo mensual (señales al {as_of})"),
        ProposalKind::Deposit => {
            format!("📋 Propuesta de compras por un depósito detectado ({as_of}, solo compras)")
        }
    };
    let mut lines = vec![title, "Pesos objetivo:".to_string()];
    for (id, w) in targets.iter() {
        lines.push(format!("• {id}: {}", pct(w)));
    }
    lines.push("Órdenes planificadas:".to_string());
    for o in &plan.orders {
        lines.push(format!(
            "• {} {}: {} a ~{} (≈ {}, comisión ≈ {})",
            side_es(o.side),
            o.asset,
            o.qty,
            ars(o.exec_price),
            ars(o.est_value),
            ars(o.est_fees)
        ));
    }
    lines.push(
        "El plan se recalcula con los precios y el efectivo del momento en que aprobás: puede diferir de lo mostrado."
            .to_string(),
    );
    lines.push(format!("Vence: {expires_at_art} (hora Argentina)."));
    lines.join("\n")
}

impl Engine {
    /// Status text for an already computed valuation.
    fn render_status(&self, v: &Valuation) -> anyhow::Result<String> {
        let mode = self.exec_mode();
        let (nav_key, dd_key, index_key) = (
            self.state_key(STATE_NAV),
            self.state_key(STATE_DRAWDOWN),
            self.state_key(STATE_RISK_INDEX),
        );
        let tracker = self.db(|s| s.get_state::<NavTracker>(&nav_key))?;
        let drawdown_state = self.db(|s| s.get_state::<DrawdownState>(&dd_key))?;
        let risk_index = self.db(|s| s.get_state::<RiskIndex>(&index_key))?;
        let nav_line = match tracker.and_then(|t| t.nav(v.equity_usd)) {
            Some(n) => format!("NAV (rendimiento, incluye el efectivo): {n:.4}"),
            None => "NAV: sin datos todavía".to_string(),
        };
        let breaker_line = match (&risk_index, &drawdown_state) {
            (Some(i), Some(d)) => format!(
                "Freno: caída de lo invertido (en dólares) desde su máximo: {}{}",
                pct(d.drawdown(i.level)),
                if d.tripped { " · ACTIVADO" } else { "" }
            ),
            _ => "Freno: sin observaciones todavía".to_string(),
        };
        let decision = self.db(|s| s.recent_decisions(1, mode))?;
        let unresolved = self.db(|s| unresolved_orders(s, mode, false))?;
        let pending = self.db(|s| s.pending_proposals())?.len();

        let mut lines = vec![
            "📊 Estado de moon-trade".to_string(),
            format!(
                "Modo: {} · En pausa: {}",
                mode_es(self.cfg.mode),
                if self.is_paused()? { "sí" } else { "no" }
            ),
            format!(
                "Patrimonio: USD {} ({}) · CCL {}",
                v.equity_usd,
                ars(v.equity_ars),
                v.ccl
            ),
            nav_line,
            breaker_line,
            "Posiciones:".to_string(),
        ];
        for p in v.positions.iter().filter(|p| p.qty > Decimal::ZERO) {
            lines.push(format!(
                "• {}: {} × {} = {} ({})",
                p.symbol,
                p.qty,
                ars(p.price_ars),
                ars(p.value_ars),
                weight_pct(p.value_ars, v.equity_ars)
            ));
        }
        lines.push(format!(
            "• Efectivo: {} ({})",
            ars(v.cash_total_ars),
            weight_pct(v.cash_total_ars, v.equity_ars)
        ));
        for (sym, qty) in &v.untracked {
            lines.push(format!("• {sym}: {qty} (fuera del universo, se ignora)"));
        }
        lines.push(match decision.first() {
            Some(d) => format!("Última decisión: {} ({})", art_date(d.at), d.kind),
            None => "Última decisión: ninguna".to_string(),
        });
        lines.push(format!(
            "Órdenes abiertas o con resultado desconocido: {} · Propuestas pendientes: {pending}",
            unresolved.len()
        ));
        lines.extend(unresolved.iter().map(order_line));
        Ok(lines.join("\n"))
    }

    pub async fn status_text(&self) -> anyhow::Result<String> {
        let v = self.valuation().await?;
        self.render_status(&v)
    }

    pub async fn weekly_report(&self) -> anyhow::Result<String> {
        let v = self.valuation().await?;
        let mut text = self.render_status(&v)?;
        let mode = self.exec_mode();
        let now = self.now();
        let week_ago = now - Duration::days(7);

        let snapshots = self.db(|s| s.snapshots_between(mode, week_ago, now))?;
        let nav_key = self.state_key(STATE_NAV);
        let current_nav = self
            .db(|s| s.get_state::<NavTracker>(&nav_key))?
            .and_then(|t| t.nav(v.equity_usd));
        let change = match (snapshots.first(), current_nav) {
            (Some(first), Some(nav)) if first.nav > 0.0 => {
                format!("{:+.2}%", (nav / first.nav - 1.0) * 100.0)
            }
            _ => "sin datos".to_string(),
        };

        let orders = self.db(|s| s.orders_between(mode, week_ago, now))?;
        let filled: Vec<_> = orders
            .iter()
            .filter(|o| o.filled_qty > Decimal::ZERO)
            .collect();
        let fees: Decimal = filled.iter().map(|o| o.fees).sum();
        let fees_label = if filled.iter().any(|o| o.fees_estimated) {
            "comisiones (estimadas)"
        } else {
            "comisiones"
        };

        // Deposits and withdrawals of the week and the equity bridge. Only flows after the first
        // snapshot count: earlier ones are already inside the initial equity. Each flow is
        // converted with the CCL of the first snapshot at or after it (fallback: the latest).
        let flow_from = snapshots.first().map_or(week_ago, |first| first.at);
        let flows: Vec<_> = self
            .db(|s| s.flows_between(mode, flow_from, now))?
            .into_iter()
            .filter(|f| f.at > flow_from)
            .collect();
        let fallback_ccl = snapshots.last().map_or(v.ccl, |last| last.ccl);
        let ccl_for = |at| {
            snapshots
                .iter()
                .find(|s| s.at >= at)
                .map_or(fallback_ccl, |s| s.ccl)
        };
        let sum_flows = |kind: FlowKind| -> (Decimal, Decimal) {
            flows.iter().filter(|f| f.kind == kind).fold(
                (Decimal::ZERO, Decimal::ZERO),
                |(ars_sum, usd_sum), f| {
                    let amount = f.amount_ars.abs();
                    let ccl = ccl_for(f.at);
                    let usd = if ccl > Decimal::ZERO {
                        amount / ccl
                    } else {
                        Decimal::ZERO
                    };
                    (ars_sum + amount, usd_sum + usd)
                },
            )
        };
        let (deposits, deposits_usd) = sum_flows(FlowKind::Deposit);
        let (withdrawals, withdrawals_usd) = sum_flows(FlowKind::Withdrawal);
        let net_usd = round_money(deposits_usd - withdrawals_usd);
        let bridge = match snapshots.first() {
            Some(first) => {
                let result = v.equity_usd - first.equity_usd - net_usd;
                format!(
                    "Puente del patrimonio (USD): inicial {} → aportes netos {:+} → resultado {:+} → final {}",
                    first.equity_usd, net_usd, result, v.equity_usd
                )
            }
            None => "Puente del patrimonio: sin snapshot inicial de la semana".to_string(),
        };

        let mut lines = vec![
            String::new(),
            "🗓️ Resumen semanal".to_string(),
            format!("Variación del NAV en 7 días: {change}"),
            format!(
                "Operaciones de la semana: {} · {fees_label}: {}",
                filled.len(),
                ars(fees)
            ),
            format!(
                "Depósitos de la semana: {} · extracciones: {}",
                ars(deposits),
                ars(withdrawals)
            ),
            bridge,
        ];
        for o in filled {
            let price = o
                .avg_fill_price
                .map_or_else(String::new, |p| format!(" a {}", ars(p)));
            lines.push(format!(
                "• {} {} {} × {}{price}",
                art_date(o.created_at),
                side_es(o.side),
                o.symbol,
                o.filled_qty
            ));
        }
        text.push_str(&lines.join("\n"));
        Ok(text)
    }
}
