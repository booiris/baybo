//! Events flowing into the TUI event loop.
//!
//! Terminal input is read directly from a `crossterm::EventStream` in the
//! loop; this enum carries the *other* asynchronous sources — outgoing
//! messages delivered by the router, dashboard snapshots from background
//! tasks, warn/error log records echoed from the tracing subscriber, and
//! explicit shutdown requests.

use aura_channels::{DashboardSnapshot, ViewKind};
use aura_model::ContentBlock;
use tokio::sync::mpsc;

/// Events consumed by the TUI's main loop (non-terminal sources).
pub(crate) enum AppEvent {
    /// Router delivered the final assistant response for the active
    /// session. Arrives after any preceding `StreamDelta` events and
    /// replaces the streaming buffer with the full content blocks.
    Outgoing(Vec<ContentBlock>),
    /// Incremental text chunk for the in-flight assistant response.
    /// Appended to the live streaming buffer shown at the tail of the
    /// scrollback until `Outgoing` finalises it.
    StreamDelta(String),
    /// A dashboard snapshot (re-)fetched after an OpenView or refresh.
    DashboardReady(ViewKind, DashboardSnapshot),
    /// A warn/error tracing event, forwarded from the subscriber for display
    /// inline with chat scrollback.
    Log(LogRecord),
    /// A tool-call approval was queued on the shared approval state. The
    /// payload carries no data — the loop reads the current head from
    /// `AppState::approval` when redrawing.
    ApprovalRequested,
    /// External shutdown or user-initiated quit.
    Shutdown,
}

/// A single log entry surfaced in the chat scrollback.
///
/// Populated by a tracing `Layer`; see `src/tui_log.rs`. Kept as a flat value
/// type so the crate does not need to depend on `tracing` internals.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

/// Level of a forwarded tracing event. Restricted to the severities the TUI
/// surfaces — lower levels stay in the log file only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Warn,
    Error,
}

/// Non-blocking sender for forwarding tracing events into the TUI.
///
/// Cloneable and `Send + Sync`. Dropping the sink is harmless — the
/// underlying channel stays alive as long as the `TuiAdapter` holds its own
/// reference to the sender.
#[derive(Clone)]
pub struct TuiLogSink {
    tx: mpsc::Sender<AppEvent>,
}

impl TuiLogSink {
    pub(crate) fn new(tx: mpsc::Sender<AppEvent>) -> Self {
        Self { tx }
    }

    /// Forward a record into the TUI event loop. Drops silently on full
    /// channel or closed receiver: logs still land in the file layer, and
    /// backpressuring a tracing callback could deadlock the emitter.
    pub fn emit(&self, record: LogRecord) {
        let _ = self.tx.try_send(AppEvent::Log(record));
    }
}
