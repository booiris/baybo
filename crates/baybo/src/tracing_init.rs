//! Shared tracing subscriber init for every entrypoint in this binary.
//!
//! Three disjoint modes: `Stdout` for one-shot argv, `File` for the
//! long-running gateway server (rolling daily log under the workspace,
//! redacted on disk), and `Tui` for the chat client, which gets both a
//! rolling file of its own and a warn/error mirror into the ratatui
//! scrollback.
//!
//! Keeping all three here means any tweak (filter default, timestamp
//! format, JSON switch, redaction wiring) lands in one place instead
//! of drifting between `main.rs` and `gateway_cmd.rs`.
//!
//! `init_tracing` installs the global subscriber and returns a
//! [`TracingGuards`] holding the non-blocking appender's `WorkerGuard`
//! when one exists — the caller must keep it alive for the process
//! lifetime, otherwise the background writer stops flushing.

use std::io::IsTerminal;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use baybo_gateway::{LogBuffer, LogBufferLayer};
use baybo_security::{LeakDetector, RedactingMakeWriter};
use baybo_tui::TuiLogSink;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::tui_log::TuiLogLayer;

/// How many recent events the in-memory `LogBuffer` keeps for `/v1/logs`.
/// Bounded so a noisy trace level can't eat unbounded memory.
const LOG_BUFFER_CAPACITY: usize = 2_000;

pub struct SecondPrecisionTimer;

impl FormatTime for SecondPrecisionTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"))
    }
}

pub enum TracingMode<'a> {
    /// One-shot argv path: fmt layer to stdout, no file, no TUI mirror.
    Stdout,
    /// One-shot `-p` prompt path: fmt layer to stderr (stdout is reserved
    /// for the assistant's answer), defaulting to `warn` so a piped
    /// `baybo -p` stays quiet unless `RUST_LOG` opts into more.
    Stderr,
    /// Gateway server path: writes rolling daily logs to
    /// `<log_dir>/baybo.log` through a [`RedactingMakeWriter`] so
    /// secrets matching any detector rule are masked on disk.
    File {
        log_dir: &'a Path,
        leak_detector: Arc<LeakDetector>,
    },
    /// TUI chat path: a rolling daily log at
    /// `<log_dir>/tui.log.YYYY-MM-DD` (its own file — the TUI is a separate
    /// process from the gateway that owns `baybo.log`), plus a
    /// [`TuiLogLayer`] mirroring warn/error events into the chat scrollback
    /// via `tui_sink` once the caller populates it.
    ///
    /// The file is what makes a TUI failure diagnosable at all: the mirror
    /// drains into the event loop, so anything logged while that loop is
    /// unwinding — the exit reason above all — reaches no reader but the
    /// file. Never fall back to stdout here; ratatui owns it.
    Tui {
        tui_sink: Arc<OnceLock<TuiLogSink>>,
        log_dir: &'a Path,
        leak_detector: Arc<LeakDetector>,
    },
}

/// Build the rolling daily writer for `<log_dir>/<prefix>.YYYY-MM-DD`, masked
/// through the shared [`LeakDetector`]. `None` when the file can't be opened —
/// each caller decides what to do without one.
///
/// Built through `RollingFileAppender::builder` rather than the `rolling::daily`
/// convenience wrapper, which `.expect()`s on the initial open: a read-only or
/// full log directory would abort the process at startup instead of degrading.
fn rolling_file_writer(
    log_dir: &Path,
    prefix: &str,
    leak_detector: Arc<LeakDetector>,
) -> Option<(RedactingMakeWriter<NonBlocking>, WorkerGuard)> {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!(
            "warning: could not create log dir {}: {e}",
            log_dir.display()
        );
        return None;
    }
    let appender = match RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .build(log_dir)
    {
        Ok(appender) => appender,
        Err(e) => {
            eprintln!(
                "warning: could not open log file {}/{prefix}: {e}",
                log_dir.display()
            );
            return None;
        }
    };
    let (writer, guard) = tracing_appender::non_blocking(appender);
    Some((RedactingMakeWriter::new(leak_detector, writer), guard))
}

pub struct TracingGuards {
    /// Non-blocking appender guard. `None` for stdout mode or when the
    /// file mode fell back to stdout after a `create_dir_all` failure.
    /// Held only for its `Drop` side-effect — dropping flushes and
    /// stops the background writer, so the caller must bind the
    /// `TracingGuards` for the lifetime of the process.
    _worker: Option<WorkerGuard>,
    /// Shared ring buffer backing `/v1/logs`. The gateway server holds
    /// an Arc into it via `GatewayDeps::log_buffer`; other entrypoints
    /// (stdout, tui) keep the buffer alive here even though they never
    /// read from it, so the layer is always valid for the dispatcher.
    log_buffer: Arc<LogBuffer>,
}

impl TracingGuards {
    /// Access the shared in-memory log buffer. Callers wire this into
    /// `GatewayDeps::log_buffer` so the admin `/v1/logs` endpoint can
    /// query the same events the subscriber is capturing.
    pub fn log_buffer(&self) -> Arc<LogBuffer> {
        Arc::clone(&self.log_buffer)
    }
}

/// Install the global tracing subscriber for this process.
///
/// Safe to call once per process — subsequent calls log a warning via
/// `eprintln!` (they don't panic). All entrypoints in this binary are
/// mutually exclusive, so the single-init invariant holds in practice.
pub fn init_tracing(mode: TracingMode<'_>) -> TracingGuards {
    // `baybo-tools`'s BashTool exports `BAYBO_HELP_AGENT=1` around every
    // CLI invocation, which is how we can tell the binary is being
    // driven by the agent loop rather than a human at a TTY. In that
    // case Stdout-mode tracing has to keep two promises: (a) stay
    // quiet so routine boot events ("registered built-in skills")
    // don't end up in the command's stdout buffer ahead of the actual
    // payload, and (b) emit no ANSI escapes — BashTool captures
    // stdout as JSON, and ESC bytes would become literal `[..]`
    // text and break downstream parsers. `RUST_LOG` always wins if
    // explicitly set.
    let agent_mode =
        std::env::var_os(baybo_cli::cli::ENV_HELP_AGENT).is_some_and(|v| !v.is_empty());
    // Keep routine boot chatter out of the way when the payload is the
    // point: agent-driven argv (stdout is captured as JSON) and the `-p`
    // prompt path (stderr is a sidebar to the streamed answer) both
    // default to `warn`. `RUST_LOG` always wins.
    let quiet =
        matches!(mode, TracingMode::Stderr) || (agent_mode && matches!(mode, TracingMode::Stdout));
    let default_filter = if quiet { "baybo=warn" } else { "baybo=info" };
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let json = std::env::var("BAYBO_LOG_FORMAT").unwrap_or_default() == "json";
    let log_buffer = LogBuffer::new(LOG_BUFFER_CAPACITY);
    let buffer_layer = LogBufferLayer::new(Arc::clone(&log_buffer));

    match mode {
        TracingMode::Stdout => {
            let fmt_layer = fmt::layer()
                .with_ansi(!agent_mode)
                .with_timer(SecondPrecisionTimer)
                .with_target(true)
                .with_file(true)
                .with_line_number(true);
            let result = if json {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(buffer_layer)
                    .with(fmt_layer.json().with_span_list(true))
                    .try_init()
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(buffer_layer)
                    .with(fmt_layer)
                    .try_init()
            };
            if let Err(e) = result {
                eprintln!("warning: tracing subscriber already initialized: {e}");
            }
            TracingGuards {
                _worker: None,
                log_buffer,
            }
        }
        TracingMode::Stderr => {
            let fmt_layer = fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal())
                .with_timer(SecondPrecisionTimer)
                .with_target(true)
                .with_file(true)
                .with_line_number(true);
            let result = if json {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(buffer_layer)
                    .with(fmt_layer.json().with_span_list(true))
                    .try_init()
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(buffer_layer)
                    .with(fmt_layer)
                    .try_init()
            };
            if let Err(e) = result {
                eprintln!("warning: tracing subscriber already initialized: {e}");
            }
            TracingGuards {
                _worker: None,
                log_buffer,
            }
        }
        TracingMode::File {
            log_dir,
            leak_detector,
        } => {
            let Some((writer, guard)) = rolling_file_writer(
                log_dir,
                baybo_workspace::paths::LOG_FILE_PREFIX,
                leak_detector,
            ) else {
                eprintln!("warning: falling back to stdout logging.");
                return init_tracing(TracingMode::Stdout);
            };
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
                    .with(buffer_layer)
                    .with(fmt_layer.json().with_span_list(true))
                    .try_init()
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(buffer_layer)
                    .with(fmt_layer)
                    .try_init()
            };
            if let Err(e) = result {
                eprintln!("warning: tracing subscriber already initialized: {e}");
            }
            TracingGuards {
                _worker: Some(guard),
                log_buffer,
            }
        }
        TracingMode::Tui {
            tui_sink,
            log_dir,
            leak_detector,
        } => {
            let file = rolling_file_writer(
                log_dir,
                baybo_workspace::paths::TUI_LOG_FILE_PREFIX,
                leak_detector,
            );
            let (writer, worker) = match file {
                Some((writer, guard)) => (Some(writer), Some(guard)),
                None => (None, None),
            };
            let fmt_layer = writer.map(|writer| {
                fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_timer(SecondPrecisionTimer)
                    .with_target(true)
                    .with_file(true)
                    .with_line_number(true)
            });
            let result = if json {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(buffer_layer)
                    .with(fmt_layer.map(|l| l.json().with_span_list(true)))
                    .with(TuiLogLayer::new(tui_sink))
                    .try_init()
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(buffer_layer)
                    .with(fmt_layer)
                    .with(TuiLogLayer::new(tui_sink))
                    .try_init()
            };
            if let Err(e) = result {
                eprintln!("warning: tracing subscriber already initialized: {e}");
            }
            TracingGuards {
                _worker: worker,
                log_buffer,
            }
        }
    }
}
