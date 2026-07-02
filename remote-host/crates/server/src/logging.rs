//! Process-wide logging: an `EnvFilter`-gated subscriber that fans out to a
//! rolling daily file under `LOG_DIR` (kept on the `/data` volume by default, so
//! it survives container recreation) **and** to stderr (so `docker compose logs`
//! keeps working). The library crates only ever call `tracing::*`; the binary
//! owns the one subscriber.
//!
//! This is a public-facing, blind relay, so the default filter is deliberately
//! quiet: dependencies at `warn`, our own crates at `info`. There is no
//! request/byte/per-connection logging on the hot path — only session-level
//! events (control connect/disconnect, limit backstops, admission changes) — so
//! the steady-state volume stays low. Operators tune verbosity with `RUST_LOG`.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

/// Where the rolling log files live when `LOG_DIR` is unset. Inside the `/data`
/// volume (bind-mounted to the host `./data`) so the logs land on the host and
/// outlive `docker compose up --build`.
const DEFAULT_LOG_DIR: &str = "/data/logs";
/// Rolled file names are `<prefix>.<date>.<suffix>`, e.g. `remote-host.2026-06-29.log`.
const LOG_FILE_PREFIX: &str = "remote-host";
const LOG_FILE_SUFFIX: &str = "log";
/// Daily files retained on disk; older ones are pruned by the appender. Bounds
/// disk use without an external logrotate.
const LOG_RETENTION_DAYS: usize = 14;
/// Default `RUST_LOG` when the env var is unset: dependencies silenced to `warn`,
/// our own crates at `info`. Every workspace crate's target is listed explicitly
/// (crate `remote-host-edge` → target `remote_host_edge`, etc.) — when a crate is
/// added or renamed, add/update its directive here rather than relying on
/// `remote_host=info` prefix-matching the rest.
const DEFAULT_LOG_FILTER: &str = "warn,remote_host=info,remote_host_relay=info,\
     remote_host_push=info,remote_host_edge=info,remote_host_admission=info,\
     remote_host_dashboard=info";

/// Install the global subscriber. Returns a [`WorkerGuard`] that the caller MUST
/// keep alive for the process lifetime — dropping it flushes and stops the
/// non-blocking file writer. Returns `None` when the log directory can't be
/// opened: logging then falls back to stderr only, and the service still runs.
#[must_use]
pub(crate) fn init() -> Option<WorkerGuard> {
    // A fresh `EnvFilter` per layer — `EnvFilter` isn't `Clone`. A blank
    // `RUST_LOG` (Docker Compose passes `${RUST_LOG:-}` as the empty string,
    // which is a *valid* empty filter that would silence every event) is treated
    // as unset so the documented default applies; an unparseable value falls
    // back too, remembered here and warned about after `.init()` (nothing can
    // log before the subscriber exists).
    let directives = std::env::var("RUST_LOG").unwrap_or_default();
    let invalid_rust_log =
        !directives.trim().is_empty() && EnvFilter::try_new(&directives).is_err();
    let filter = || {
        if directives.trim().is_empty() {
            EnvFilter::new(DEFAULT_LOG_FILTER)
        } else {
            EnvFilter::try_new(&directives).unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER))
        }
    };

    let log_dir = std::env::var("LOG_DIR").unwrap_or_else(|_| DEFAULT_LOG_DIR.to_string());

    let guard = match build_appender(&log_dir) {
        Ok(appender) => {
            let (file_writer, guard) = tracing_appender::non_blocking(appender);
            tracing_subscriber::registry()
                .with(filter())
                .with(fmt::layer().with_writer(file_writer).with_ansi(false))
                .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false))
                .init();
            tracing::info!(dir = %log_dir, "remote-host: file logging enabled");
            Some(guard)
        }
        Err(e) => {
            tracing_subscriber::registry()
                .with(filter())
                .with(fmt::layer().with_writer(std::io::stderr).with_ansi(false))
                .init();
            tracing::warn!(
                dir = %log_dir,
                error = %e,
                "remote-host: file logging disabled (could not open log dir); stderr only"
            );
            None
        }
    };
    if invalid_rust_log {
        tracing::warn!(
            rust_log = %directives,
            fallback_filter = DEFAULT_LOG_FILTER,
            "remote-host: RUST_LOG is set but unparseable; falling back to the default filter"
        );
    }
    guard
}

/// Build the daily-rolling, retention-capped file appender, creating `dir` first.
fn build_appender(dir: &str) -> Result<RollingFileAppender, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(dir)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix(LOG_FILE_SUFFIX)
        .max_log_files(LOG_RETENTION_DAYS)
        .build(dir)?;
    Ok(appender)
}
