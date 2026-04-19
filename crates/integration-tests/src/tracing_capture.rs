//! Per-test tracing capture without touching the global subscriber.
//!
//! Each `capture_tracing()` returns a `TracingCapture` that — while
//! held — installs a thread-local `Dispatch` and records every event
//! emitted on this thread. Assertions look at the captured `Vec` after
//! the test has driven its work. The capture is dropped at end of
//! scope and the previous dispatch is restored.

use std::sync::Arc;

use parking_lot::Mutex;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::Registry;
use tracing_subscriber::{Layer, util::SubscriberInitExt};

#[derive(Debug, Clone)]
pub struct CapturedEvent {
    pub level: Level,
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
}

impl CapturedEvent {
    /// True if any captured field (including `message`) contains the
    /// given substring.
    pub fn contains(&self, needle: &str) -> bool {
        if self.message.contains(needle) {
            return true;
        }
        self.fields.iter().any(|(_, v)| v.contains(needle))
    }
}

#[derive(Default)]
struct CaptureLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

struct FieldVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s;
        } else {
            self.fields.push((field.name().to_string(), s));
        }
    }
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = FieldVisitor {
            message: String::new(),
            fields: Vec::new(),
        };
        event.record(&mut visitor);
        self.events.lock().push(CapturedEvent {
            level: *metadata.level(),
            target: metadata.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        });
    }
}

/// RAII guard that restores the previous global subscriber on drop.
/// Hold it for the duration of the test; query `events()` to assert.
pub struct TracingCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl TracingCapture {
    pub fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().to_vec()
    }

    pub fn at_level(&self, level: Level) -> Vec<CapturedEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.level == level)
            .collect()
    }

    pub fn any_contains(&self, needle: &str) -> bool {
        self.events().iter().any(|e| e.contains(needle))
    }
}

/// Install a thread-local capturing subscriber. Use:
/// ```ignore
/// let cap = capture_tracing();
/// // ... run code under test ...
/// assert!(cap.any_contains("aws_access_key"));
/// ```
pub fn capture_tracing() -> TracingCapture {
    let events: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let layer = CaptureLayer {
        events: events.clone(),
    };
    let subscriber = Registry::default().with(layer);
    let guard = subscriber.set_default();
    TracingCapture {
        events,
        _guard: guard,
    }
}
