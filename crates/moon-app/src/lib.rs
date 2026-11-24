//! moon-trade application: configuration, CLI and commands.

pub mod cli;
pub mod commands;
pub mod config;
pub mod logging;
pub mod report;

use cli::{Cli, Command, DataCmd};
use config::AppConfig;

const PENDING: &str = "pendiente (tarea 12/13)";

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Init => commands::init::run(&cli.config),
        Command::Data {
            cmd: DataCmd::Sync { from },
        } => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::data::run_sync(&cfg, from).await
        }
        Command::Backtest {
            include_holdout,
            json,
        } => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::backtest::run_backtest_command(&cfg, include_holdout, json)
        }
        Command::BacktestRuns => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::backtest::run_backtest_runs(&cfg)
        }
        Command::Plan
        | Command::Status
        | Command::Paper { .. }
        | Command::Pause
        | Command::Resume
        | Command::Serve
        | Command::Check { .. } => {
            println!("{PENDING}");
            Ok(())
        }
    }
}
