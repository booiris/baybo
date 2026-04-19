//! Shared tracing subscriber init for every entrypoint in this binary.
//!
//! Three disjoint modes: `Stdout` for one-shot argv, `File` for the
//! long-running gateway server (rolling daily log under the workspace,
//! redacted on disk), and `Tui` for the chat client (warn/error mirrored
//! into the ratatui scrollback — no file, since the TUI is a thin client
//! of the gateway which already owns the authoritative log).
//!
//! Keeping all three here means any tweak (filter default, timestamp
//! format, JSON switch, redaction wiring) lands in one place instead
//! of drifting between `main.rs` and `gateway_cmd.rs`.
//!
//! `init_tracing` installs the global subscriber and returns a
//! [`TracingGuards`] holding the non-blocking appender's `WorkerGuard`
//! when one exists — the caller must keep it alive for the process
//! lifetime, otherwise the background writer stops flushing.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use aura_security::{LeakDetector, RedactingMakeWriter};
use aura_tui::TuiLogSink;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::tui_log::TuiLogLayer;

pub struct SecondPrecisionTimer;

impl FormatTime for SecondPrecisionTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"))
    }
}

pub enum TracingMode<'a> {
    /// One-shot argv path: fmt layer to stdout, no file, no TUI mirror.
    Stdout,
    /// Gateway server path: writes rolling daily logs to
    /// `<log_dir>/aura.log` through a [`RedactingMakeWriter`] so
    /// secrets matching any detector rule are masked on disk.
    File {
        log_dir: &'a Path,
        leak_detector: Arc<LeakDetector>,
    },
    /// TUI chat path: no file on disk (the TUI is a thin gateway
    /// client — the gateway owns the authoritative log file), just a
    /// [`TuiLogLayer`] that mirrors warn/error events into the chat
    /// scrollback via `tui_sink` once the caller populates it. Info
    /// and below are silently dropped because ratatui owns stdout.
    Tui { tui_sink: Arc<OnceLock<TuiLogSink>> },
}

pub struct TracingGuards {
    /// Non-blocking appender guard. `None` for stdout mode or when the
    /// file mode fell back to stdout after a `create_dir_all` failure.
    /// Held only for its `Drop` side-effect — dropping flushes and
    /// stops the background writer, so the caller must bind the
    /// `TracingGuards` for the lifetime of the process.
    _worker: Option<WorkerGuard>,
}

/// Install the global tracing subscriber for this process.
///
/// Safe to call once per process — subsequent calls log a warning via
/// `eprintln!` (they don't panic). All entrypoints in this binary are
/// mutually exclusive, so the single-init invariant holds in practice.
pub fn init_tracing(mode: TracingMode<'_>) -> TracingGuards {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aura=info"));
    let json = std::env::var("AURA_LOG_FORMAT").unwrap_or_default() == "json";

    match mode {
        TracingMode::Stdout => {
            let fmt_layer = fmt::layer()
                .with_timer(SecondPrecisionTimer)
                .with_target(true)
                .with_file(true)
                .with_line_number(true);
            let result = if json {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer.json().with_span_list(true))
                    .try_init()
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer)
                    .try_init()
            };
            if let Err(e) = result {
                eprintln!("warning: tracing subscriber already initialized: {e}");
            }
            TracingGuards { _worker: None }
        }
        TracingMode::File {
            log_dir,
            leak_detector,
        } => {
            if let Err(e) = std::fs::create_dir_all(log_dir) {
                eprintln!(
                    "warning: could not create log dir {}: {e}. Falling back to stdout logging.",
                    log_dir.display()
                );
                return init_tracing(TracingMode::Stdout);
            }
            let appender = tracing_appender::rolling::daily(log_dir, "aura.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let writer = RedactingMakeWriter::new(leak_detector, writer);
            let fmt_layer = fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_timer(SecondPrecisionTimer)
                .with_target(true)
                .with_file(true)
                .with_line_number(true);
            let result = if json {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer.json().with_span_list(true))
                    .try_init()
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer)
                    .try_init()
            };
            if let Err(e) = result {
                eprintln!("warning: tracing subscriber already initialized: {e}");
            }
            TracingGuards {
                _worker: Some(guard),
            }
        }
        TracingMode::Tui { tui_sink } => {
            let result = tracing_subscriber::registry()
                .with(env_filter)
                .with(TuiLogLayer::new(tui_sink))
                .try_init();
            if let Err(e) = result {
                eprintln!("warning: tracing subscriber already initialized: {e}");
            }
            TracingGuards { _worker: None }
        }
    }
}
