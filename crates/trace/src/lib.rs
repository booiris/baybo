//! Trace types, row conversions, and span lifecycle — see
//! `docs/modules/trace.md` for the design.
//!
//! Hierarchy: `Session > Turn > Step > Span (+ SpanEvent)`. `Session`
//! lives in `baybo-model`; `Turn` lives in `baybo-turn`; this crate owns
//! `Step`, `Span`, `SpanEvent`, the `SpanRecorder` persistence
//! orchestrator, and the row conversions that persist them.
//!
//! The `TraceStore` trait lives in `baybo-store` (the ports crate) and
//! trades in `StepRow` / `SpanRow` / `SpanEventRow`; this crate owns the
//! `to_row` / `from_row` conversions and converts at the recorder
//! boundary, while `baybo-storage` provides the sqlite implementation.

mod error;
mod event;
mod outcome;
mod recorder;
mod span;
mod step;
mod store;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use baybo_store::{SpanEventRow, SpanRow, StepRow, ToolSetRow, TraceStore};
pub use error::TraceError;
pub use event::{SPAN_EVENT_TEXT_MAX_BYTES, SpanEvent, SpanEventKind, ToolEventPayload};
pub use outcome::{LifecycleOutcome, LifecycleState};
pub use recorder::{SpanRecorder, TraceEvent, TraceEventStream};
pub use span::{
    LlmCallBegin, LlmCallInputs, LlmCallResult, LlmToolCallRecord, LlmToolDefinition, LlmToolSet,
    LlmToolSetRef, PersistedToolCallOutput, Span, SpanFinalize, SpanHandle, SpanKind,
    ToolCallBegin, ToolCallOrigin, ToolCallOutput, ToolCallResult,
};
pub use step::{CompressionTrigger, Step, StepHandle, StepKind};

pub type Result<T> = std::result::Result<T, TraceError>;
