//! Spanish texts for notifications, status and the weekly report.

use chrono::Duration;
use moon_core::nav::NavTracker;
use moon_core::rebalance::RebalancePlan;
use moon_core::risk::DrawdownState;
use moon_core::time::art_date;
use moon_core::types::{OrderReason, OrderStatus, Side};
use moon_core::weights::TargetWeights;
use rust_decimal::Decimal;

use super::{Engine, ExecutionReport, STATE_DRAWDOWN, STATE_NAV, UNKNOWN_OUTCOME, Valuation};
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
            "• {} {}: {} de {} a {} — {}; ejecutado {}; comisión {}{est}",
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
    as_of: chrono::NaiveDate,
    targets: &TargetWeights,
    plan: &RebalancePlan,
    expires_at_art: &str,
) -> String {
    let mut lines = vec![
        format!("📋 Propuesta de rebalanceo mensual (señales al {as_of})"),
        "Pesos objetivo:".to_string(),
    ];
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
    lines.push(format!("Vence: {expires_at_art} (hora Argentina)."));
    lines.join("\n")
}

impl Engine {
    /// Status text for an already computed valuation.
    fn render_status(&self, v: &Valuation) -> anyhow::Result<String> {
        let mode = self.exec_mode();
        let tracker = self.db(|s| s.get_state::<NavTracker>(STATE_NAV))?;
        let drawdown_state = self.db(|s| s.get_state::<DrawdownState>(STATE_DRAWDOWN))?;
        let nav = tracker.and_then(|t| t.nav(v.equity_usd));
        let nav_line = match nav {
            Some(n) => {
                let dd = drawdown_state.map_or(0.0, |d| d.drawdown(n));
                format!("NAV: {n:.4} · caída desde el máximo: {}", pct(dd))
            }
            None => "NAV: sin datos todavía".to_string(),
        };
        let decision = self.db(|s| s.recent_decisions(1))?;
        let open_orders = self.db(|s| s.open_orders(mode))?.len();
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
            ars(v.cash_ars),
            weight_pct(v.cash_ars, v.equity_ars)
        ));
        for (sym, qty) in &v.untracked {
            lines.push(format!("• {sym}: {qty} (fuera del universo, se ignora)"));
        }
        lines.push(match decision.first() {
            Some(d) => format!("Última decisión: {} ({})", art_date(d.at), d.kind),
            None => "Última decisión: ninguna".to_string(),
        });
        lines.push(format!(
            "Órdenes abiertas: {open_orders} · Propuestas pendientes: {pending}"
        ));
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
        let current_nav = self
            .db(|s| s.get_state::<NavTracker>(STATE_NAV))?
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

        let mut lines = vec![
            String::new(),
            "🗓️ Resumen semanal".to_string(),
            format!("Variación del NAV en 7 días: {change}"),
            format!(
                "Operaciones de la semana: {} · comisiones: {}",
                filled.len(),
                ars(fees)
            ),
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
