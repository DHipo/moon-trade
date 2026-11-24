//! Entry point of the `moon` binary.

use clap::Parser;
use moon_app::cli::Cli;
use moon_app::config::AppConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // The guard must live until the end of `main` so file logs are flushed.
    let log_dir = AppConfig::load(&cli.config).ok().map(|c| c.log_dir);
    let _guard = moon_app::logging::init_logging(log_dir.as_deref())?;
    moon_app::run(cli).await
}
