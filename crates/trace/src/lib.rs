//! Trace types, store trait, and span lifecycle — see
//! `docs/modules/trace.md` for the design.
//!
//! Hierarchy: `Session > Job > Step > Span (+ SpanEvent)`. `Session`
//! lives in `aura-model`; `Job` lives in `aura-job`; this crate owns
//! `Step`, `Span`, `SpanEvent`, the `TraceStore` trait, and the
//! `SpanRecorder` persistence orchestrator.
//!
//! `aura-storage` provides the libsql implementation of `TraceStore`;
//! the trait itself lives here so downstream callers and tests can
//! depend on `aura-trace` alone for trace-recording work.

mod error;
mod event;
mod outcome;
mod recorder;
mod span;
mod step;
mod store;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::TraceError;
pub use event::{SpanEvent, SpanEventKind, ToolEventPayload};
pub use outcome::{LifecycleOutcome, LifecycleState};
pub use recorder::{SpanRecorder, TraceEvent, TraceEventStream};
pub use span::{
    LlmCallBegin, LlmCallInputs, LlmCallResult, LlmToolCallRecord, Span, SpanFinalize, SpanHandle,
    SpanKind, ToolCallBegin, ToolCallOrigin, ToolCallResult,
};
pub use step::{Step, StepHandle, StepKind};
pub use store::TraceStore;

pub type Result<T> = std::result::Result<T, TraceError>;
