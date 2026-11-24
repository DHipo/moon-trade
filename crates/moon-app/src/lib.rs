//! moon-trade application: configuration, CLI and commands.

pub mod cli;
pub mod commands;
pub mod config;
pub mod engine;
pub mod logging;
pub mod notify;
pub mod report;
pub mod scheduler;
pub mod serve;
pub mod telegram_bot;

use cli::{CheckCmd, Cli, Command, DataCmd, OrdersCmd, PaperCmd, RiskCmd};
use config::{AppConfig, load_secrets};

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
        Command::Plan => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::plan::run_plan(cfg, &load_secrets()).await
        }
        Command::Status => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::status::run_status(cfg, &load_secrets()).await
        }
        Command::Paper {
            cmd: PaperCmd::Deposit { amount_ars },
        } => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::paper::run_deposit(cfg, &load_secrets(), amount_ars).await
        }
        Command::Paper {
            cmd: PaperCmd::Reset,
        } => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::paper::run_reset(cfg, &load_secrets()).await
        }
        Command::Orders {
            cmd:
                OrdersCmd::Resolve {
                    order_id,
                    outcome,
                    qty,
                    price,
                },
        } => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::orders::run_resolve(&cfg, &order_id, outcome, qty, price)
        }
        Command::Orders {
            cmd: OrdersCmd::List { unknown },
        } => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::orders::run_list(&cfg, unknown)
        }
        Command::Risk {
            cmd: RiskCmd::Accept { symbol },
        } => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::risk::run_accept(&cfg, &symbol)
        }
        Command::Pause => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::pause::run_set_paused(&cfg, &load_secrets(), true).await
        }
        Command::Resume => {
            let cfg = AppConfig::load(&cli.config)?;
            commands::pause::run_set_paused(&cfg, &load_secrets(), false).await
        }
        Command::Serve => {
            let cfg = AppConfig::load(&cli.config)?;
            let (engine, telegram) = commands::build_engine(cfg, &load_secrets())?;
            serve::serve(std::sync::Arc::new(engine), telegram).await
        }
        Command::Check { cmd: CheckCmd::Iol } => {
            let cfg = AppConfig::load(&cli.config)?;
            let text = commands::check::check_iol(&cfg, &load_secrets()).await?;
            println!("{text}");
            Ok(())
        }
        Command::Check {
            cmd: CheckCmd::Telegram,
        } => {
            println!(
                "{}",
                commands::check::check_telegram(&load_secrets()).await?
            );
            Ok(())
        }
    }
}
