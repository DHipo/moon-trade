//! Text reports (Spanish).

use moon_core::metrics::Metrics;

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
        "{:<20}{:>14}\n{:<20}{:>14}\n{:<20}{:>14}\n",
        "Operaciones",
        strategy.trades,
        "Comisiones (USD)",
        strategy.total_fees,
        "Capital final (USD)",
        strategy.final_equity
    ));
    out
}
