//! Command-line interface definition.

use std::path::PathBuf;

use chrono::NaiveDate;
use clap::{Parser, Subcommand};

use crate::commands::orders::ResolveOutcome;
use rust_decimal::Decimal;

#[derive(Parser)]
#[command(
    name = "moon",
    version,
    about = "moon-trade: administrador de cartera personal"
)]
pub struct Cli {
    #[arg(long, default_value = "moon.toml")]
    pub config: PathBuf,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Crea la base de datos y los directorios necesarios
    Init,
    /// Datos de mercado
    Data {
        #[command(subcommand)]
        cmd: DataCmd,
    },
    /// Corre el backtest y lo registra
    Backtest {
        /// Incluye el período virgen (invalida la validación final)
        #[arg(long)]
        include_holdout: bool,
        /// Salida en JSON
        #[arg(long)]
        json: bool,
    },
    /// Lista los backtests registrados
    BacktestRuns,
    Plan,
    Status,
    Paper {
        #[command(subcommand)]
        cmd: PaperCmd,
    },
    /// Órdenes
    Orders {
        #[command(subcommand)]
        cmd: OrdersCmd,
    },
    /// Índice de riesgo
    Risk {
        #[command(subcommand)]
        cmd: RiskCmd,
    },
    Pause,
    Resume,
    Serve,
    Check {
        #[command(subcommand)]
        cmd: CheckCmd,
    },
}

#[derive(Subcommand)]
pub enum DataCmd {
    /// Descarga historial de Yahoo a la base
    Sync {
        #[arg(long)]
        from: Option<NaiveDate>,
    },
}

#[derive(Subcommand)]
pub enum PaperCmd {
    Deposit { amount_ars: Decimal },
    Reset,
}

#[derive(Subcommand)]
pub enum CheckCmd {
    Iol,
    Telegram,
}

#[derive(Subcommand)]
pub enum RiskCmd {
    /// El posible cambio de ratio pendiente fue un cambio de ratio: se rebasa sin contar caída
    /// (se aplica en el próximo control de riesgo del servicio; no corre ningún control)
    Ratio {
        /// Símbolo con el posible cambio de ratio (el que muestra el aviso o `moon status`)
        symbol: String,
    },
    /// El posible cambio de ratio pendiente fue un movimiento real: se cuenta desde la base
    /// (se aplica en el próximo control de riesgo del servicio; no corre ningún control)
    Drop {
        /// Símbolo con el posible cambio de ratio (el que muestra el aviso o `moon status`)
        symbol: String,
    },
}

#[derive(Subcommand)]
pub enum OrdersCmd {
    /// Lista las órdenes abiertas o con resultado desconocido, con su identificador corto
    List {
        /// Solo las de resultado desconocido
        #[arg(long)]
        unknown: bool,
    },
    /// Registra a mano el resultado de una orden con resultado desconocido, después de verificarlo
    /// en IOL (solo modifica la base local: no llama a IOL)
    Resolve {
        /// Identificador de la orden o un prefijo único de al menos 6 caracteres (el que muestran
        /// los avisos y `moon orders list`)
        order_id: String,
        /// Qué pasó en IOL: filled (ejecutada) o cancelled (inexistente o cancelada)
        outcome: ResolveOutcome,
        /// Cantidad ejecutada (por defecto la de la orden; en cancelled, para una ejecución parcial)
        #[arg(long)]
        qty: Option<Decimal>,
        /// Precio promedio de ejecución (por defecto el límite de la orden)
        #[arg(long)]
        price: Option<Decimal>,
    },
}
