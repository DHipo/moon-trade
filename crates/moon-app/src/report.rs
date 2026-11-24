//! Text reports (Spanish).

use moon_core::metrics::Metrics;

use crate::config::AppConfig;

fn pct(x: f64) -> String {
    format!("{:.2}%", x * 100.0)
}

fn num(x: f64) -> String {
    format!("{x:.2}")
}

/// Side-by-side metrics table for the strategy and (optionally) its benchmark.
pub fn format_metrics_table(strategy: &Metrics, benchmark: Option<&Metrics>) -> String {
    type Row = (&'static str, fn(&Metrics) -> String);
    let rows: [Row; 10] = [
        ("Retorno total", |m| pct(m.total_return)),
        ("CAGR", |m| pct(m.cagr)),
        ("Volatilidad", |m| pct(m.volatility)),
        ("Sharpe", |m| num(m.sharpe)),
        ("Máxima caída", |m| pct(m.max_drawdown)),
        ("Calmar", |m| num(m.calmar)),
        ("Mejor mes", |m| pct(m.best_month)),
        ("Peor mes", |m| pct(m.worst_month)),
        ("Meses positivos", |m| pct(m.positive_months)),
        ("Tiempo en refugio", |m| pct(m.time_in_refuge)),
    ];
    let mut out = format!(
        "Período: {} a {} ({:.1} años)\n",
        strategy.start, strategy.end, strategy.years
    );
    match benchmark {
        Some(_) => out.push_str(&format!(
            "{:<20}{:>14}{:>14}\n",
            "", "Estrategia", "Benchmark"
        )),
        None => out.push_str(&format!("{:<20}{:>14}\n", "", "Estrategia")),
    }
    for (label, f) in rows {
        out.push_str(&format!("{:<20}{:>14}", label, f(strategy)));
        if let Some(b) = benchmark {
            out.push_str(&format!("{:>14}", f(b)));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "{:<20}{:>14}\n{:<20}{:>14}\n{:<20}{:>14}\n{:<20}{:>14}\n{:<20}{:>14}\n",
        "Operaciones",
        strategy.trades,
        "Rotación anual",
        pct(strategy.turnover),
        "Comisiones (USD)",
        strategy.total_fees,
        "Aportes netos (USD)",
        strategy.net_contributions,
        "Capital final (USD)",
        strategy.final_equity
    ));
    out
}

/// Warning shown with a backtest whose refuge is CASH simulated without any loss in dollars.
pub fn cash_refuge_warning(cfg: &AppConfig) -> Option<String> {
    let refuge_is_cash = cfg.universe().ok().is_some_and(|u| u.refuge().id.is_cash());
    (refuge_is_cash && cfg.backtest.config.cash_usd_return == 0.0).then(|| {
        "⚠️ El refugio CASH se simula en dólares sin pérdida; en vivo son pesos (configurá backtest.cash_usd_return)"
            .to_string()
    })
}
