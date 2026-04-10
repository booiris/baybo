use async_trait::async_trait;
use aura_trace::{SessionTrace, TraceError, TraceFilter, TraceNode, TraceNodeId};

pub type Result<T> = std::result::Result<T, TraceError>;

/// Persistence interface for trace data.
#[async_trait]
pub trait TraceStore: Send + Sync {
    async fn save_trace(&self, trace: &SessionTrace) -> Result<()>;
    async fn load_trace(&self, session_id: &str) -> Result<Option<SessionTrace>>;
    async fn query_traces(&self, filter: TraceFilter) -> Result<Vec<SessionTrace>>;
    async fn load_node(&self, session_id: &str, node_id: &TraceNodeId)
    -> Result<Option<TraceNode>>;
}
