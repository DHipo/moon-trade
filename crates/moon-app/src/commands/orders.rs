//! `moon orders resolve`: records by hand the outcome of an order whose result was unknown.
//!
//! Purely local: it edits the stored order after the owner checked IOL and never calls the broker.

use anyhow::{anyhow, bail};
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use moon_core::fees::FeeModel;
use moon_core::money::round_money;
use moon_core::types::{ExecMode, OrderStatus};
use moon_store::Store;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::config::{AppConfig, TradingMode};
use crate::engine::UNKNOWN_OUTCOME;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ResolveOutcome {
    /// La orden se ejecutó en IOL
    Filled,
    /// La orden no existe o se canceló en IOL
    Cancelled,
}

/// Applies the owner's verdict to the stored order and returns a Spanish description of the
/// change. Only orders that are still open or whose outcome is unknown can be resolved.
pub fn resolve_order(
    store: &mut Store,
    fees: &FeeModel,
    now: DateTime<Utc>,
    id: Uuid,
    outcome: ResolveOutcome,
    qty: Option<Decimal>,
    price: Option<Decimal>,
) -> anyhow::Result<String> {
    let mut order = store
        .get_order(id)?
        .ok_or_else(|| anyhow!("No existe la orden {id}."))?;
    let unknown = order.status == OrderStatus::Failed
        && order
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with(UNKNOWN_OUTCOME));
    if order.status.is_terminal() && !unknown {
        bail!(
            "La orden {id} ya está resuelta (estado {}): no se modifica.",
            crate::engine::status_es(order.status)
        );
    }
    if let Some(q) = qty
        && (q <= Decimal::ZERO || q > order.qty)
    {
        bail!(
            "--qty debe ser mayor que 0 y no superar la cantidad de la orden ({}).",
            order.qty
        );
    }
    if let Some(p) = price
        && p <= Decimal::ZERO
    {
        bail!("--price debe ser mayor que 0.");
    }
    let before = format!(
        "{} {} {} × {}",
        crate::engine::status_es(order.status),
        order.symbol,
        order.side.as_str(),
        order.qty
    );

    let note = "resuelta a mano con `moon orders resolve`";
    let executed = match outcome {
        ResolveOutcome::Filled => {
            let q = qty.unwrap_or(order.qty);
            let p = price.or(order.limit_price).ok_or_else(|| {
                anyhow!("La orden era a mercado: indicá el precio de ejecución con --price.")
            })?;
            Some((q, p))
        }
        ResolveOutcome::Cancelled => match (qty, price) {
            (Some(q), Some(p)) => Some((q, p)),
            (None, None) => None,
            _ => bail!("Para una ejecución parcial indicá --qty y --price juntos."),
        },
    };

    let msg = match executed {
        Some((q, p)) => {
            order.filled_qty = q;
            order.avg_fill_price = Some(p);
            order.fees = fees.total(round_money(q * p));
            order.fees_estimated = true;
            order.status = if q >= order.qty {
                OrderStatus::Filled
            } else {
                // Partly executed, the rest is gone: terminal either way.
                OrderStatus::Cancelled
            };
            format!(
                "Orden {id} ({before}) marcada como {}: {q} a {p}, comisión estimada {}.",
                if order.status == OrderStatus::Filled {
                    "ejecutada"
                } else {
                    "ejecutada parcialmente y cancelada"
                },
                order.fees
            )
        }
        None => {
            order.filled_qty = Decimal::ZERO;
            order.avg_fill_price = None;
            order.status = OrderStatus::Cancelled;
            format!("Orden {id} ({before}) marcada como cancelada, sin ejecución.")
        }
    };
    order.error = Some(note.to_string());
    order.updated_at = now;
    store.update_order(&order)?;
    Ok(msg)
}

/// Minimum length of an order id prefix accepted by `moon orders resolve`.
const MIN_PREFIX_LEN: usize = 6;

/// Finds the order whose id is `input` (full UUID) or the unique order starting with it.
pub fn find_order_id(store: &Store, input: &str) -> anyhow::Result<Uuid> {
    let input = input.trim();
    if let Ok(id) = Uuid::parse_str(input) {
        return Ok(id);
    }
    if input.len() < MIN_PREFIX_LEN {
        bail!(
            "El identificador debe tener al menos {MIN_PREFIX_LEN} caracteres (usá el que muestra `moon orders list`)."
        );
    }
    let candidates = store.orders_by_id_prefix(input)?;
    match candidates.as_slice() {
        [] => bail!("No existe ninguna orden cuyo identificador empiece con «{input}»."),
        [one] => Ok(one.id),
        many => {
            let list = many
                .iter()
                .map(crate::engine::order_line)
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "El prefijo «{input}» es ambiguo, coincide con {} órdenes:\n{list}\nUsá un prefijo más largo.",
                many.len()
            )
        }
    }
}

/// Text of `moon orders list`.
pub fn list_text(store: &Store, mode: ExecMode, unknown_only: bool) -> anyhow::Result<String> {
    let orders = crate::engine::unresolved_orders(store, mode, unknown_only)?;
    if orders.is_empty() {
        return Ok(if unknown_only {
            "No hay órdenes con resultado desconocido.".to_string()
        } else {
            "No hay órdenes abiertas ni con resultado desconocido.".to_string()
        });
    }
    let mut lines = vec![format!("{} orden(es):", orders.len())];
    lines.extend(orders.iter().map(crate::engine::order_line));
    Ok(lines.join("\n"))
}

/// CLI entry of `moon orders list`: read-only.
pub fn run_list(cfg: &AppConfig, unknown_only: bool) -> anyhow::Result<()> {
    let store = Store::open(&cfg.database_path)?;
    let mode = match cfg.mode {
        TradingMode::Paper => ExecMode::Paper,
        TradingMode::LiveConfirm | TradingMode::LiveAuto => ExecMode::Live,
    };
    println!("{}", list_text(&store, mode, unknown_only)?);
    Ok(())
}

/// CLI entry: opens the database of the configuration and resolves the order. Never calls IOL.
pub fn run_resolve(
    cfg: &AppConfig,
    id: &str,
    outcome: ResolveOutcome,
    qty: Option<Decimal>,
    price: Option<Decimal>,
) -> anyhow::Result<()> {
    let mut store = Store::open(&cfg.database_path)?;
    let id = find_order_id(&store, id)?;
    let msg = resolve_order(&mut store, &cfg.fees, Utc::now(), id, outcome, qty, price)?;
    println!("{msg}");
    Ok(())
}
