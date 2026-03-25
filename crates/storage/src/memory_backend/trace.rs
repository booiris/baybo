use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use aura_core::Result;
use aura_trace::{SessionTrace, TraceFilter, TraceNode, TraceNodeId, TraceStore};

/// In-memory implementation of [`TraceStore`].
pub struct InMemoryTraceStore {
    /// Key: session_id
    data: Arc<RwLock<HashMap<String, SessionTrace>>>,
}

impl InMemoryTraceStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryTraceStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TraceStore for InMemoryTraceStore {
    async fn save_trace(&self, trace: &SessionTrace) -> Result<()> {
        let mut guard = self.data.write().await;
        guard.insert(trace.session_id.clone(), trace.clone());
        Ok(())
    }

    async fn load_trace(&self, session_id: &str) -> Result<Option<SessionTrace>> {
        let guard = self.data.read().await;
        Ok(guard.get(session_id).cloned())
    }

    async fn query_traces(&self, filter: TraceFilter) -> Result<Vec<SessionTrace>> {
        let guard = self.data.read().await;
        let results = guard
            .values()
            .filter(|t| {
                if let Some(ref sid) = filter.session_id
                    && t.session_id != *sid
                {
                    return false;
                }
                // Check time bounds using the root node's started_at.
                if let Some(root_node) = t.nodes.get(&t.root) {
                    let ts = root_node.span.started_at;
                    if let Some(ref from) = filter.from
                        && ts < *from
                    {
                        return false;
                    }
                    if let Some(ref to) = filter.to
                        && ts >= *to
                    {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();
        Ok(results)
    }

    async fn load_node(
        &self,
        session_id: &str,
        node_id: &TraceNodeId,
    ) -> Result<Option<TraceNode>> {
        let guard = self.data.read().await;
        let node = guard
            .get(session_id)
            .and_then(|trace| trace.nodes.get(node_id))
            .cloned();
        Ok(node)
    }
}
