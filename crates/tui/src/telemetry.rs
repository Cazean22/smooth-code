use std::io::IsTerminal;

use anyhow::{Context, Result};
use cazean_config::Config;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, writer::BoxMakeWriter},
    prelude::*,
};

pub(crate) struct TelemetryGuard {
    _log_writer_guard: Option<WorkerGuard>,
}

pub(crate) fn init(config: &Config) -> Result<TelemetryGuard> {
    let interactive_tui = std::io::stdout().is_terminal();
    let force_terminal_logs = config.telemetry.force_stderr;

    // Log to the user-global `~/.cazean/logs/<file>` when running an interactive
    // TUI; fall back to stderr when forced, non-interactive, or the home
    // directory can't be resolved (mirrors config's silently-skip behavior).
    let log_path = (interactive_tui && !force_terminal_logs)
        .then(|| cazean_config::log_path(&config.telemetry.log_file_name))
        .flatten();

    let (writer, log_writer_guard) = if let Some(log_path) = log_path {
        let log_dir = log_path
            .parent()
            .context("telemetry log path is missing a parent directory")?;
        std::fs::create_dir_all(log_dir).with_context(|| {
            format!(
                "failed to create telemetry log directory at {}",
                log_dir.display()
            )
        })?;

        let file_name = log_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("telemetry log file name must be valid UTF-8")?;
        let appender = tracing_appender::rolling::never(log_dir, file_name);
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);
        (BoxMakeWriter::new(non_blocking), Some(guard))
    } else {
        (BoxMakeWriter::new(std::io::stderr), None)
    };

    let console_layer = console_subscriber::ConsoleLayer::builder()
        .with_default_env()
        .spawn();
    let log_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.telemetry.log_filter))?;
    let log_layer = fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true)
        .with_file(true)
        .with_line_number(true)
        .with_filter(log_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(log_layer)
        .init();

    tracing::info!(interactive_tui, force_terminal_logs, "tracing initialized");

    Ok(TelemetryGuard {
        _log_writer_guard: log_writer_guard,
    })
}
