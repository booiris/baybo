use std::collections::HashMap;
use std::sync::Arc;

use aura_job::OperationKind;
use aura_storage::TraceStore;
use aura_trace::tree::{attach_child, create_root_node, set_active_leaf};
use aura_trace::{
    ExecutionProvenance, SessionTrace, SpanHandle, SpanInput, SpanResult, TraceError, TraceNodeId,
};

/// Collects trace spans for a single session and persists them via a `TraceStore`.
pub struct TraceCollector {
    session_trace: SessionTrace,
    store: Arc<dyn TraceStore>,
}

type Result<T> = std::result::Result<T, TraceError>;

impl TraceCollector {
    pub fn new(session_id: &str, store: Arc<dyn TraceStore>) -> Self {
        let (root_id, root_node) = create_root_node(session_id);
        let mut nodes = HashMap::new();
        nodes.insert(root_id.clone(), root_node);

        let session_trace = SessionTrace {
            session_id: session_id.to_owned(),
            root: root_id.clone(),
            nodes,
            forks: Vec::new(),
            active_leaf: root_id,
        };

        Self {
            session_trace,
            store,
        }
    }

    pub fn begin_span(
        &mut self,
        kind: OperationKind,
        job_id: Option<&str>,
        provenance: ExecutionProvenance,
        input: SpanInput,
    ) -> SpanHandle {
        let parent_id = self.session_trace.active_leaf.clone();

        let input_clone = input.clone();
        let provenance_clone = provenance.clone();
        let node_id = match attach_child(
            &mut self.session_trace,
            &parent_id,
            kind,
            job_id,
            provenance,
            input,
        ) {
            Ok(id) => id,
            Err(_) => {
                let fallback_parent = self.session_trace.root.clone();
                let session_id = self.session_trace.session_id.clone();
                attach_child(
                    &mut self.session_trace,
                    &fallback_parent,
                    OperationKind::UserMessageHandling { session_id },
                    job_id,
                    provenance_clone,
                    input_clone,
                )
                .unwrap_or_else(|_| self.session_trace.root.clone())
            }
        };

        set_active_leaf(&mut self.session_trace, node_id.clone());

        SpanHandle { node_id }
    }

    pub fn end_span(&mut self, handle: SpanHandle, result: SpanResult) {
        if let Some(node) = self.session_trace.nodes.get_mut(&handle.node_id) {
            node.span.ended_at = Some(chrono::Utc::now());
            node.span.result = Some(result);

            if let Some(ref parent_id) = node.parent.clone() {
                set_active_leaf(&mut self.session_trace, parent_id.clone());
            }
        }
    }

    pub async fn flush(&self) -> Result<()> {
        self.store.save_trace(&self.session_trace).await
    }

    /// Snapshot the store handle and the current `SessionTrace` for async
    /// flushing. Used by callers that hold the collector behind a sync lock
    /// (e.g. `ObservabilityRecorder`) to avoid awaiting while the guard
    /// is held.
    pub fn flush_snapshot(&self) -> (Arc<dyn TraceStore>, SessionTrace) {
        (Arc::clone(&self.store), self.session_trace.clone())
    }

    pub fn active_leaf(&self) -> &TraceNodeId {
        &self.session_trace.active_leaf
    }
}

#[cfg(test)]
impl TraceCollector {
    fn session_trace(&self) -> &SessionTrace {
        &self.session_trace
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use aura_trace::{SpanResult, TraceFilter, TraceNode};
    use parking_lot::Mutex;

    struct MemoryTraceStore {
        traces: Mutex<HashMap<String, SessionTrace>>,
    }

    impl MemoryTraceStore {
        fn new() -> Self {
            Self {
                traces: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl TraceStore for MemoryTraceStore {
        async fn save_trace(&self, trace: &SessionTrace) -> Result<()> {
            self.traces
                .lock()
                .insert(trace.session_id.clone(), trace.clone());
            Ok(())
        }

        async fn load_trace(&self, session_id: &str) -> Result<Option<SessionTrace>> {
            Ok(self.traces.lock().get(session_id).cloned())
        }

        async fn query_traces(&self, _filter: TraceFilter) -> Result<Vec<SessionTrace>> {
            Ok(self.traces.lock().values().cloned().collect())
        }

        async fn load_node(
            &self,
            session_id: &str,
            node_id: &TraceNodeId,
        ) -> Result<Option<TraceNode>> {
            Ok(self
                .traces
                .lock()
                .get(session_id)
                .and_then(|t| t.nodes.get(node_id).cloned()))
        }
    }

    fn make_collector() -> TraceCollector {
        let store = Arc::new(MemoryTraceStore::new());
        TraceCollector::new("sess-1", store)
    }

    #[test]
    fn begin_and_end_span() {
        let mut collector = make_collector();

        let handle = collector.begin_span(
            OperationKind::LlmCall {
                model: "gpt-4".to_owned(),
            },
            Some("job-1"),
            ExecutionProvenance::default(),
            SpanInput::None,
        );

        let node_id = handle.node_id.clone();
        collector.end_span(
            handle,
            SpanResult::LlmResponse {
                output_content: "hello".to_owned(),
                input_tokens: 10,
                output_tokens: 5,
                thinking: None,
                tool_calls: Vec::new(),
                latency: std::time::Duration::from_millis(100),
            },
        );

        let node = collector.session_trace().nodes.get(&node_id).unwrap();
        assert!(node.span.ended_at.is_some());
        assert!(node.span.result.is_some());
    }

    #[tokio::test]
    async fn flush_persists_trace() {
        let store = Arc::new(MemoryTraceStore::new());
        let collector = TraceCollector::new("sess-flush", store.clone());
        collector.flush().await.unwrap();

        let loaded = store.load_trace("sess-flush").await.unwrap();
        assert!(loaded.is_some());
    }
}
