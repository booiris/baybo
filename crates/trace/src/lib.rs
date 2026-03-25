pub mod collector;
pub mod fork;
pub mod snapshot;
pub mod tree;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use aura_context::ContextSnapshot;
use aura_core::{ChatMessage, ContentBlock, OperationKind};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::collector::TraceCollector;

/// Unique identifier for a node in the trace tree.
pub type TraceNodeId = String;

/// The full trace tree for a single session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTrace {
    pub session_id: String,
    pub root: TraceNodeId,
    pub nodes: HashMap<TraceNodeId, TraceNode>,
    pub forks: Vec<ForkRecord>,
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

/// Sanitized result recorded for a span.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpanResult {
    LlmResponse {
        output_preview: String,
        input_tokens: usize,
        output_tokens: usize,
        reasoning_redacted: bool,
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

/// Record of a trace fork (branch) operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkRecord {
    pub id: String,
    pub from_node: TraceNodeId,
    pub fork_root: TraceNodeId,
    pub reason: String,
    pub created_at: DateTime<Utc>,
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

/// Persistence interface for trace data.
#[async_trait]
pub trait TraceStore: Send + Sync {
    /// Save the entire session trace atomically.
    async fn save_trace(&self, trace: &SessionTrace) -> aura_core::Result<()>;

    /// Load the trace tree for a session.
    async fn load_trace(&self, session_id: &str) -> aura_core::Result<Option<SessionTrace>>;

    /// Query traces matching the given filter.
    async fn query_traces(&self, filter: TraceFilter) -> aura_core::Result<Vec<SessionTrace>>;

    /// Load a single node from a session trace.
    async fn load_node(
        &self,
        session_id: &str,
        node_id: &TraceNodeId,
    ) -> aura_core::Result<Option<TraceNode>>;
}
