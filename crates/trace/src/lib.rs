//! Trace types, row conversions, and span lifecycle — see
//! `docs/modules/trace.md` for the design.
//!
//! Hierarchy: `Session > Job > Step > Span (+ SpanEvent)`. `Session`
//! lives in `baybo-model`; `Job` lives in `baybo-job`; this crate owns
//! `Step`, `Span`, `SpanEvent`, the `SpanRecorder` persistence
//! orchestrator, and the row conversions that persist them.
//!
//! The `TraceStore` trait lives in `baybo-store` (the ports crate) and
//! trades in `StepRow` / `SpanRow` / `SpanEventRow`; this crate owns the
//! `to_row` / `from_row` conversions and converts at the recorder
//! boundary, while `baybo-storage` provides the libsql implementation.

mod error;
mod event;
mod outcome;
mod recorder;
mod span;
mod step;
mod store;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use baybo_store::{SpanEventRow, SpanRow, StepRow, TraceStore};
pub use error::TraceError;
pub use event::{SpanEvent, SpanEventKind, ToolEventPayload};
pub use outcome::{LifecycleOutcome, LifecycleState};
pub use recorder::{SpanRecorder, TraceEvent, TraceEventStream};
pub use span::{
    GoalSteeringAudit, GoalSteeringKind, LlmCallBegin, LlmCallInputs, LlmCallResult,
    LlmToolCallRecord, Span, SpanFinalize, SpanHandle, SpanKind, ToolCallBegin, ToolCallOrigin,
    ToolCallResult,
};
pub use step::{Step, StepHandle, StepKind};

pub type Result<T> = std::result::Result<T, TraceError>;
