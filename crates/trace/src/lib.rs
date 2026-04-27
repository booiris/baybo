pub mod error;
pub mod fork;
pub mod snapshot;
pub mod tree;

use std::collections::HashMap;
use std::time::Duration;

use aura_context::ContextSnapshot;
use aura_job::OperationKind;
use aura_model::{ChatMessage, ContentBlock};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use error::TraceError;

pub type Result<T> = std::result::Result<T, TraceError>;

/// Unique identifier for a node in the trace tree.
pub type TraceNodeId = String;

/// The full trace tree for a single session.
///
/// In-session branches (used by the rollback path) live in the tree
/// itself via `parent`/`children` pointers. Cross-session forks (user
/// "branch this conversation") are now expressed via
/// `aura_model::Session::parent_link`, not on this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTrace {
    pub session_id: String,
    pub root: TraceNodeId,
    pub nodes: HashMap<TraceNodeId, TraceNode>,
    pub active_leaf: TraceNodeId,
}

/// A single node in the trace tree, representing one operation span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceNode {
    pub id: TraceNodeId,
    pub parent: Option<TraceNodeId>,
    pub children: Vec<TraceNodeId>,
    pub span: TraceSpan,
    pub context_snapshot: Option<ContextSnapshot>,
    /// ReAct-iteration grouping. All nodes produced within the same
    /// agent-loop iteration share this id (one LLM node + 0..K tool
    /// nodes). Defaults to the node id when no grouping is recorded
    /// (e.g. legacy trace data).
    #[serde(default)]
    pub span_id: String,
    /// Zero-based index of this span within the owning Job. Increments
    /// by one per ReAct iteration. `0` for legacy nodes.
    #[serde(default)]
    pub span_index: u32,
    /// What role this node plays inside its span.
    #[serde(default)]
    pub span_role: SpanRole,
}

/// What kind of action a `TraceNode` represents inside its span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpanRole {
    /// The LLM call that opens the iteration.
    Llm,
    /// A tool call dispatched by the iteration's LLM call.
    Tool,
    /// A system action (compression, memory write, acceptance event, …)
    /// that does not fit the LLM/Tool roles.
    #[default]
    System,
}

/// Details of a traced operation span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSpan {
    pub kind: OperationKind,
    pub job_id: Option<String>,
    pub provenance: ExecutionProvenance,
    pub input: SpanInput,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub result: Option<SpanResult>,
}

/// Records which versions of code and configuration were active during a span.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionProvenance {
    pub model_id: Option<String>,
    pub provider: Option<String>,
    pub provider_config_hash: Option<String>,
    pub skill_version: Option<String>,
    pub tool_artifact_hash: Option<String>,
    pub soul_version: Option<String>,
}

/// Sanitized input recorded for a span.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpanInput {
    UserInput {
        content: Vec<ContentBlock>,
    },
    LlmCall {
        input_messages: Vec<ChatMessage>,
        temperature: Option<f32>,
    },
    ToolExecution {
        parameters: Value,
    },
    SkillExecution {
        args: Vec<String>,
    },
    ContextCompression {
        before_tokens: usize,
    },
    MemoryOperation {
        query: Option<String>,
    },
    None,
}

/// A tool call recorded in a trace span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolCallRecord {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Sanitized result recorded for a span.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpanResult {
    LlmResponse {
        output_content: String,
        input_tokens: usize,
        output_tokens: usize,
        thinking: Option<String>,
        tool_calls: Vec<LlmToolCallRecord>,
        latency: Duration,
    },
    ToolResult {
        output: Value,
        success: bool,
        latency: Duration,
    },
    SkillResult {
        output: String,
    },
    ContextCompressionResult {
        after_tokens: usize,
        summary: Option<String>,
    },
    FinalResponse {
        content: String,
    },
    Error {
        error: String,
    },
}

/// Handle returned by `begin_span` to later complete the span with `end_span`.
#[derive(Debug, Clone)]
pub struct SpanHandle {
    pub node_id: TraceNodeId,
}

/// Filter criteria for querying traces.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceFilter {
    pub session_id: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}
