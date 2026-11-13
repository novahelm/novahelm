//! Structured JSON-lines logging: daily-rotated files (7 kept) plus os_log on macOS.

use anyhow::Result;
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::{
    EnvFilter, Layer, Registry, layer::SubscriberExt, util::SubscriberInitExt,
};

/// Number of daily log files retained.
pub const KEEP_FILES: usize = 7;

/// Install the global subscriber. Keep the returned guard alive to flush on exit.
/// Filter defaults to `info`; `NOVALINK_LOG` overrides it.
pub fn init(log_dir: &Path) -> Result<WorkerGuard> {
    crate::paths::create_private_dir(log_dir)?;
    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("novalink-agent")
        .filename_suffix("jsonl")
        .max_log_files(KEEP_FILES)
        .build(log_dir)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter =
        || EnvFilter::try_from_env("NOVALINK_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let json = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_writer(writer)
        .with_filter(filter());

    let reg = Registry::default().with(json);
    #[cfg(target_os = "macos")]
    let reg = reg
        .with(tracing_oslog::OsLogger::new("com.novalink.agent", "default").with_filter(filter()));

    reg.try_init()?;
    Ok(guard)
}
