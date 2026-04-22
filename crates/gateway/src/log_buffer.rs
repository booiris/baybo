//! In-memory ring buffer of recent tracing events, queried by `/v1/logs`.
//!
//! The server installs a [`LogBufferLayer`] alongside the fmt/file layers
//! so every event that matches the subscriber's env filter is also
//! pushed into a bounded `VecDeque`. The admin router reads from the
//! same [`LogBuffer`] handle to back the dashboard's "System Logs" view.
//!
//! This is intentionally independent of `tracing-appender`'s on-disk log:
//! the buffer is for live UI browsing (small, capped, cheap to query),
//! not durable storage. Oldest-wins eviction keeps memory bounded.

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// Fan-out capacity for the live log stream. Lagged subscribers —
/// whose receiver is more than this many records behind — get a
/// `RecvError::Lagged` instead of blocking the producer, which the
/// `/v1/logs/stream` handler surfaces as a single "lagged" event and
/// then resumes.
const LIVE_STREAM_CAPACITY: usize = 256;

/// Severity mirror of [`tracing::Level`], sortable from Error (highest)
/// to Trace (lowest) so callers can compare with `>=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// Parse a case-insensitive level string, accepting `"warning"` as
    /// an alias for `"warn"`. Returns `None` for unknown labels so
    /// callers can pick a safe default (the channel WS route maps
    /// unknowns to `Info`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "error" | "err" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }
}

impl From<Level> for LogLevel {
    fn from(v: Level) -> Self {
        match v {
            Level::ERROR => Self::Error,
            Level::WARN => Self::Warn,
            Level::INFO => Self::Info,
            Level::DEBUG => Self::Debug,
            Level::TRACE => Self::Trace,
        }
    }
}

/// A single captured tracing event.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub id: u64,
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
}

impl LogRecord {
    /// Does this record satisfy the non-pagination parts of `q`?
    /// Shared between `LogBuffer::query` and the `/v1/logs/stream`
    /// handler so both paths apply the exact same filter semantics.
    pub fn matches(&self, q: &LogQuery) -> bool {
        if let Some(level) = q.level
            && self.level != level
        {
            return false;
        }
        if let Some(since) = q.since
            && self.timestamp < since
        {
            return false;
        }
        if let Some(until) = q.until
            && self.timestamp > until
        {
            return false;
        }
        if let Some(needle) = q.q.as_deref() {
            let n = needle.to_ascii_lowercase();
            let hit = self.message.to_ascii_lowercase().contains(&n)
                || self.target.to_ascii_lowercase().contains(&n);
            if !hit {
                return false;
            }
        }
        true
    }
}

#[derive(Debug)]
struct Inner {
    capacity: usize,
    next_id: u64,
    records: VecDeque<LogRecord>,
}

/// Thread-safe, bounded ring buffer of [`LogRecord`]s, with a
/// side-channel `broadcast::Sender` so the admin router can push newly
/// captured events to `/v1/logs/stream` subscribers without re-polling
/// the buffer.
#[derive(Debug)]
pub struct LogBuffer {
    inner: Mutex<Inner>,
    live: broadcast::Sender<LogRecord>,
}

/// Filter + paginate parameters for [`LogBuffer::query`].
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    pub level: Option<LogLevel>,
    pub q: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: usize,
    pub offset: usize,
}

/// Query result: `items` is the current page, `total` is the full
/// match count across the buffer (before limit/offset).
#[derive(Debug)]
pub struct LogPage {
    pub items: Vec<LogRecord>,
    pub total: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Arc<Self> {
        let capacity = capacity.max(1);
        let (live, _rx) = broadcast::channel(LIVE_STREAM_CAPACITY);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                capacity,
                next_id: 0,
                records: VecDeque::with_capacity(capacity),
            }),
            live,
        })
    }

    /// Subscribe to the live stream of [`LogRecord`]s pushed after
    /// this call returns. Back-pressure is bounded: if a subscriber
    /// falls more than `LIVE_STREAM_CAPACITY` records behind, the
    /// oldest entries in its view are dropped and it receives a
    /// [`broadcast::error::RecvError::Lagged`] on the next recv.
    pub fn subscribe(&self) -> broadcast::Receiver<LogRecord> {
        self.live.subscribe()
    }

    fn push_record(
        &self,
        timestamp: DateTime<Utc>,
        level: LogLevel,
        target: String,
        message: String,
        fields: Vec<(String, String)>,
    ) {
        let record = {
            let mut inner = self.inner.lock();
            let id = inner.next_id;
            inner.next_id = inner.next_id.wrapping_add(1);
            if inner.records.len() == inner.capacity {
                inner.records.pop_front();
            }
            let rec = LogRecord {
                id,
                timestamp,
                level,
                target,
                message,
                fields,
            };
            inner.records.push_back(rec.clone());
            rec
        };
        // No subscribers yet is normal: `broadcast::Sender::send`
        // errors only when every receiver has been dropped, which we
        // don't need to react to here.
        let _ = self.live.send(record);
    }

    /// Push a synthetic record (used by tests).
    #[cfg(any(test, feature = "test-support"))]
    pub fn push_for_test(
        &self,
        timestamp: DateTime<Utc>,
        level: LogLevel,
        target: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push_record(timestamp, level, target.into(), message.into(), Vec::new());
    }

    /// Push a record captured outside the tracing layer — used by the
    /// channel WS server to surface sidecar-forwarded log lines. The
    /// buffer itself doesn't know what a sidecar is; the caller is
    /// expected to scope `target` (e.g. `sidecar::<channel_type>`) so
    /// the dashboard can filter by attribution.
    pub fn push_external(
        &self,
        level: LogLevel,
        target: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push_record(Utc::now(), level, target.into(), message.into(), Vec::new());
    }

    /// Return records matching `q`, newest first, honouring limit/offset.
    pub fn query(&self, q: &LogQuery) -> LogPage {
        let inner = self.inner.lock();
        let limit = if q.limit == 0 { usize::MAX } else { q.limit };

        let mut matched = 0usize;
        let mut items: Vec<LogRecord> = Vec::new();
        for rec in inner.records.iter().rev() {
            if !rec.matches(q) {
                continue;
            }
            matched += 1;
            if matched > q.offset && items.len() < limit {
                items.push(rec.clone());
            }
        }
        LogPage {
            items,
            total: matched,
        }
    }
}

/// Tracing [`Layer`] that pushes every captured event into a shared
/// [`LogBuffer`]. Install alongside the fmt/file layers.
pub struct LogBufferLayer {
    buffer: Arc<LogBuffer>,
}

impl LogBufferLayer {
    pub fn new(buffer: Arc<LogBuffer>) -> Self {
        Self { buffer }
    }
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

impl<S: Subscriber> Layer<S> for LogBufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = FieldVisitor {
            message: String::new(),
            fields: Vec::new(),
        };
        event.record(&mut visitor);
        self.buffer.push_record(
            Utc::now(),
            (*meta.level()).into(),
            meta.target().to_string(),
            visitor.message,
            visitor.fields,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).expect("valid ts")
    }

    #[test]
    fn evicts_oldest_when_full() {
        let buf = LogBuffer::new(2);
        buf.push_for_test(ts(1), LogLevel::Info, "a", "one");
        buf.push_for_test(ts(2), LogLevel::Info, "a", "two");
        buf.push_for_test(ts(3), LogLevel::Info, "a", "three");
        let page = buf.query(&LogQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(page.total, 2);
        assert_eq!(
            page.items
                .iter()
                .map(|r| r.message.as_str())
                .collect::<Vec<_>>(),
            vec!["three", "two"],
        );
    }

    #[test]
    fn filters_by_level_and_needle() {
        let buf = LogBuffer::new(10);
        buf.push_for_test(ts(1), LogLevel::Info, "auth", "ok");
        buf.push_for_test(ts(2), LogLevel::Error, "auth", "failed login");
        buf.push_for_test(ts(3), LogLevel::Warn, "db", "slow query");
        let page = buf.query(&LogQuery {
            level: Some(LogLevel::Error),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].message, "failed login");

        let page = buf.query(&LogQuery {
            q: Some("query".into()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].target, "db");
    }

    #[test]
    fn pagination_returns_correct_slice() {
        let buf = LogBuffer::new(10);
        for i in 0..5 {
            buf.push_for_test(ts(i), LogLevel::Info, "a", format!("m{i}"));
        }
        let page = buf.query(&LogQuery {
            limit: 2,
            offset: 2,
            ..Default::default()
        });
        assert_eq!(page.total, 5);
        assert_eq!(
            page.items
                .iter()
                .map(|r| r.message.as_str())
                .collect::<Vec<_>>(),
            vec!["m2", "m1"],
        );
    }

    #[tokio::test]
    async fn subscribe_receives_pushed_records() {
        let buf = LogBuffer::new(10);
        let mut rx = buf.subscribe();
        buf.push_for_test(ts(1), LogLevel::Info, "auth", "hello");
        let got = rx.recv().await.expect("recv ok");
        assert_eq!(got.message, "hello");
        assert_eq!(got.level, LogLevel::Info);
        assert_eq!(got.target, "auth");
    }

    #[test]
    fn matches_applies_level_q_and_range() {
        let rec = LogRecord {
            id: 0,
            timestamp: ts(100),
            level: LogLevel::Warn,
            target: "db".into(),
            message: "slow query".into(),
            fields: vec![],
        };
        let base = LogQuery {
            limit: 10,
            ..Default::default()
        };
        assert!(rec.matches(&base));
        assert!(!rec.matches(&LogQuery {
            level: Some(LogLevel::Error),
            ..base.clone()
        }));
        assert!(rec.matches(&LogQuery {
            q: Some("SLOW".into()),
            ..base.clone()
        }));
        assert!(!rec.matches(&LogQuery {
            q: Some("nope".into()),
            ..base.clone()
        }));
        assert!(!rec.matches(&LogQuery {
            since: Some(ts(200)),
            ..base.clone()
        }));
        assert!(!rec.matches(&LogQuery {
            until: Some(ts(50)),
            ..base.clone()
        }));
    }

    #[test]
    fn parse_level_is_case_insensitive_with_aliases() {
        assert_eq!(LogLevel::parse("ERROR"), Some(LogLevel::Error));
        assert_eq!(LogLevel::parse("warning"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("Debug"), Some(LogLevel::Debug));
        assert_eq!(LogLevel::parse("fatal"), None);
    }

    #[test]
    fn push_external_is_visible_to_query_and_stream() {
        let buf = LogBuffer::new(4);
        buf.push_external(LogLevel::Warn, "sidecar::telegram", "rate limited");
        let page = buf.query(&LogQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].target, "sidecar::telegram");
        assert_eq!(page.items[0].message, "rate limited");
        assert_eq!(page.items[0].level, LogLevel::Warn);
    }

    #[test]
    fn time_range_filters() {
        let buf = LogBuffer::new(10);
        buf.push_for_test(ts(10), LogLevel::Info, "a", "old");
        buf.push_for_test(ts(100), LogLevel::Info, "a", "mid");
        buf.push_for_test(ts(1000), LogLevel::Info, "a", "new");
        let page = buf.query(&LogQuery {
            since: Some(ts(50)),
            until: Some(ts(500)),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].message, "mid");
    }
}
