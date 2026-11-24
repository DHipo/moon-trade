//! Logging: stdout plus a daily rolling file in `log_dir`.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initializes logging. The returned guard must be kept alive for the whole program,
/// otherwise buffered file logs are lost.
pub fn init_logging(log_dir: Option<&Path>) -> anyhow::Result<Option<WorkerGuard>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stdout = tracing_subscriber::fmt::layer().with_target(false);
    let registry = tracing_subscriber::registry().with(filter).with(stdout);
    match log_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            let appender = tracing_appender::rolling::daily(dir, "moon.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer);
            registry.with(file).try_init().ok();
            Ok(Some(guard))
        }
        None => {
            registry.try_init().ok();
            Ok(None)
        }
    }
}
