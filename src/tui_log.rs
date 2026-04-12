//! Bridge from `tracing` events into the TUI scrollback.
//!
//! In chat mode, stdout is owned by ratatui in raw mode — writing tracing
//! records there would corrupt the frame. Instead, the file layer captures
//! everything at the configured filter level, and this layer mirrors
//! warn/error events into the chat scrollback so users still see them.
//!
//! The sink is plumbed in after `TuiAdapter::new()`, so tracing is wired
//! through a shared `OnceLock` that the layer reads on every event. Until
//! the sink lands, warn/error records still reach the file layer; they
//! just don't appear in the TUI.
//!
//! The tracing callback is synchronous, so the sink forwards with
//! `try_send` — a full channel drops the record rather than stalling the
//! emitter. File logs remain authoritative.

use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use aura_channels::{LogLevel, LogRecord, TuiLogSink};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

pub struct TuiLogLayer {
    sink: Arc<OnceLock<TuiLogSink>>,
}

impl TuiLogLayer {
    pub fn new(sink: Arc<OnceLock<TuiLogSink>>) -> Self {
        Self { sink }
    }
}

impl<S: Subscriber> Layer<S> for TuiLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = match *event.metadata().level() {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            _ => return,
        };
        let Some(sink) = self.sink.get() else { return };

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let mut message = visitor.message;
        if !visitor.fields.is_empty() {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(&visitor.fields);
        }

        sink.emit(LogRecord {
            level,
            target: event.metadata().target().to_string(),
            message,
        });
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(&mut self.fields, "{}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(&mut self.fields, "{}={:?}", field.name(), value);
        }
    }
}
