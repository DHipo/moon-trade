//! Command-line interface definition.

use std::path::PathBuf;

use chrono::NaiveDate;
use clap::{Parser, Subcommand};
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
