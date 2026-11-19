//! Logging to `%LOCALAPPDATA%\ferrofs\logs` (plus stderr in debug builds).
//! The level is controlled by the `FERROFS_LOG` env var (default `info`).

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

/// Keep the returned guard alive until exit so buffered lines are flushed.
pub fn init() -> Result<WorkerGuard> {
    let dir = crate::paths::log_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create log directory {}", dir.display()))?;

    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("ferro-link")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .context("cannot create log file")?;
    let (file_writer, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_env("FERROFS_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = cfg!(debug_assertions).then(|| fmt::layer().with_writer(std::io::stderr));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
        .with(stderr_layer)
        .try_init()
        .context("cannot install tracing subscriber")?;
    Ok(guard)
}
