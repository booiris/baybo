//! Trace domain types — see `docs/modules/trace.md` for the design.
//!
//! Hierarchy: `Session > Job > Step > Span (+ SpanEvent)`. `Session`
//! lives in `aura-model`; `Job` lives in `aura-job`; this crate owns
//! `Step`, `Span`, and `SpanEvent` plus the recovery utility.
//!
//! No persistence here — `agent::trace::SpanRecorder` (in `aura-agent`)
//! owns lifecycle and writes; `aura-storage` defines the `TraceStore`
//! trait.

mod error;
mod event;
mod outcome;
mod recovery;
mod span;
mod step;

pub use error::TraceError;
pub use event::{SpanEvent, SpanEventKind};
pub use outcome::LifecycleOutcome;
pub use recovery::{RecoveredSpan, RecoveryReport};
pub use span::{LlmToolCallRecord, Span, SpanHandle, SpanKind, SpanResult, ToolCallOrigin};
pub use step::{Step, StepHandle, StepKind};

pub type Result<T> = std::result::Result<T, TraceError>;
