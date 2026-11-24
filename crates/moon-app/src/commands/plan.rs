use moon_core::time::art_date;
use moon_core::types::Side;

use crate::config::{AppConfig, Secrets};
use crate::engine::last_day_of_previous_month;

/// Prints the current decision and the rebalance plan. Nothing is executed.
pub async fn run_plan(cfg: AppConfig, secrets: &Secrets) -> anyhow::Result<()> {
    let (engine, _) = super::build_engine(cfg, secrets)?;
    let today = art_date(engine.deps().clock.now());
    let as_of = last_day_of_previous_month(today);
    let decision = engine.decide(as_of)?;
    println!("Decisión con señales al {as_of}:");
    for (asset, w) in decision.final_weights.iter() {
        println!("  {asset}: {:.1}%", w * 100.0);
    }
    let v = engine.valuation().await?;
    println!(
        "Patrimonio: USD {} (ARS {}) · CCL {}",
        v.equity_usd,
        v.equity_ars.round_dp(2),
        v.ccl
    );
    let plan = engine.plan(&decision.final_weights, &v, false)?;
    if plan.is_empty() {
        println!("Sin cambios: la cartera ya está en línea con el objetivo.");
        return Ok(());
    }
    println!("Órdenes planificadas (no se ejecutan):");
    for o in &plan.orders {
        let side = match o.side {
            Side::Buy => "compra",
            Side::Sell => "venta",
        };
        println!(
            "  {side} {}: {} a ~ARS {} (≈ ARS {}, comisión ≈ ARS {})",
            o.asset, o.qty, o.exec_price, o.est_value, o.est_fees
        );
    }
    println!(
        "Efectivo estimado luego: ARS {}",
        plan.est_cash_after.round_dp(2)
    );
    Ok(())
}
