//! TraceCollector: span lifecycle management and persistence.

use std::collections::HashMap;
use std::sync::Arc;

use aura_context::ContextSnapshot;
use aura_core::OperationKind;

use crate::tree::{attach_child, create_root_node, set_active_leaf};
use crate::{
    ExecutionProvenance, SessionTrace, SpanHandle, SpanInput, SpanResult, TraceNodeId, TraceStore,
};

/// Collects trace spans for a single session and persists them via a `TraceStore`.
pub struct TraceCollector {
    session_trace: SessionTrace,
    store: Arc<dyn TraceStore>,
    auto_snapshot: bool,
    snapshot_interval: usize,
    spans_since_snapshot: usize,
}

impl TraceCollector {
    /// Create a new collector for the given session.
    ///
    /// A root node is created immediately. If `auto_snapshot` is true, a
    /// context snapshot placeholder will be expected every `snapshot_interval`
    /// spans (the caller is responsible for attaching the actual snapshot data).
    pub fn new(
        session_id: &str,
        store: Arc<dyn TraceStore>,
        auto_snapshot: bool,
        snapshot_interval: usize,
    ) -> Self {
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
            auto_snapshot,
            snapshot_interval,
            spans_since_snapshot: 0,
        }
    }

    /// Start a new span under the current `active_leaf`.
    ///
    /// Returns a `SpanHandle` that must be passed to `end_span` when the
    /// operation completes.
    pub fn begin_span(
        &mut self,
        kind: OperationKind,
        job_id: Option<&str>,
        provenance: ExecutionProvenance,
        input: SpanInput,
    ) -> SpanHandle {
        let parent_id = self.session_trace.active_leaf.clone();

        // attach_child only fails if the parent is missing, which should not
        // happen here since active_leaf always references a valid node. If it
        // does, we create a detached node under root as a fallback.
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
                // root always exists; fall back to root id as last resort
                .unwrap_or_else(|_| self.session_trace.root.clone())
            }
        };

        set_active_leaf(&mut self.session_trace, node_id.clone());
        self.spans_since_snapshot += 1;

        SpanHandle { node_id }
    }

    /// Complete a previously started span by recording its result and end time.
    pub fn end_span(&mut self, handle: SpanHandle, result: SpanResult) {
        if let Some(node) = self.session_trace.nodes.get_mut(&handle.node_id) {
            node.span.ended_at = Some(chrono::Utc::now());
            node.span.result = Some(result);

            // After ending, move active_leaf back to the parent so that
            // sibling spans are created at the correct level.
            if let Some(ref parent_id) = node.parent.clone() {
                set_active_leaf(&mut self.session_trace, parent_id.clone());
            }
        }
    }

    /// Persist the current session trace to the store.
    pub async fn flush(&self) -> aura_core::Result<()> {
        self.store.save_trace(&self.session_trace).await
    }

    /// Whether the auto-snapshot policy says a snapshot should be taken now.
    pub fn should_auto_snapshot(&self) -> bool {
        self.auto_snapshot
            && crate::snapshot::should_snapshot(self.spans_since_snapshot, self.snapshot_interval)
    }

    /// Attach a context snapshot to the specified node and reset the snapshot counter.
    pub fn attach_snapshot(
        &mut self,
        node_id: &TraceNodeId,
        snapshot: ContextSnapshot,
    ) -> aura_core::Result<()> {
        let node = self
            .session_trace
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| aura_core::AuraError::NotFound(format!("trace node {node_id}")))?;
        node.context_snapshot = Some(snapshot);
        self.spans_since_snapshot = 0;
        Ok(())
    }

    /// Get the current active leaf node id.
    pub fn active_leaf(&self) -> &TraceNodeId {
        &self.session_trace.active_leaf
    }
}

#[cfg(test)]
impl TraceCollector {
    fn session_trace(&self) -> &SessionTrace {
        &self.session_trace
    }

    fn fork_from(&mut self, from_node: TraceNodeId) -> aura_core::Result<String> {
        use crate::fork::fork_from as do_fork;
        let fork_id = do_fork(&mut self.session_trace, from_node, "")?;
        Ok(fork_id)
    }

    fn get_snapshot_at(&self, node_id: TraceNodeId) -> aura_core::Result<ContextSnapshot> {
        use crate::snapshot::find_nearest_snapshot;
        find_nearest_snapshot(&self.session_trace, &node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SpanResult, TraceFilter, TraceNode};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// In-memory store for testing.
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
        async fn save_trace(&self, trace: &SessionTrace) -> aura_core::Result<()> {
            let mut map = self
                .traces
                .lock()
                .map_err(|e| aura_core::AuraError::Internal(anyhow::anyhow!("{e}")))?;
            map.insert(trace.session_id.clone(), trace.clone());
            Ok(())
        }

        async fn load_trace(&self, session_id: &str) -> aura_core::Result<Option<SessionTrace>> {
            let map = self
                .traces
                .lock()
                .map_err(|e| aura_core::AuraError::Internal(anyhow::anyhow!("{e}")))?;
            Ok(map.get(session_id).cloned())
        }

        async fn query_traces(&self, _filter: TraceFilter) -> aura_core::Result<Vec<SessionTrace>> {
            let map = self
                .traces
                .lock()
                .map_err(|e| aura_core::AuraError::Internal(anyhow::anyhow!("{e}")))?;
            Ok(map.values().cloned().collect())
        }

        async fn load_node(
            &self,
            session_id: &str,
            node_id: &TraceNodeId,
        ) -> aura_core::Result<Option<TraceNode>> {
            let map = self
                .traces
                .lock()
                .map_err(|e| aura_core::AuraError::Internal(anyhow::anyhow!("{e}")))?;
            Ok(map
                .get(session_id)
                .and_then(|t| t.nodes.get(node_id).cloned()))
        }
    }

    fn make_collector() -> TraceCollector {
        let store = Arc::new(MemoryTraceStore::new());
        TraceCollector::new("sess-1", store, true, 3)
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
                output_preview: "hello".to_owned(),
                input_tokens: 10,
                output_tokens: 5,
                reasoning_redacted: false,
                latency: std::time::Duration::from_millis(100),
            },
        );

        let node = collector.session_trace().nodes.get(&node_id).unwrap();
        assert!(node.span.ended_at.is_some());
        assert!(node.span.result.is_some());
    }

    #[test]
    fn auto_snapshot_triggers_at_interval() {
        let mut collector = make_collector();

        for _ in 0..2 {
            let h = collector.begin_span(
                OperationKind::ToolExecution {
                    tool_name: "t".to_owned(),
                },
                None,
                ExecutionProvenance::default(),
                SpanInput::None,
            );
            collector.end_span(
                h,
                SpanResult::SkillResult {
                    output: String::new(),
                },
            );
        }
        assert!(!collector.should_auto_snapshot());

        let h = collector.begin_span(
            OperationKind::ToolExecution {
                tool_name: "t".to_owned(),
            },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );
        collector.end_span(
            h,
            SpanResult::SkillResult {
                output: String::new(),
            },
        );
        assert!(collector.should_auto_snapshot());
    }

    #[tokio::test]
    async fn flush_persists_trace() {
        let store = Arc::new(MemoryTraceStore::new());
        let collector = TraceCollector::new("sess-flush", store.clone(), false, 0);
        collector.flush().await.unwrap();

        let loaded = store.load_trace("sess-flush").await.unwrap();
        assert!(loaded.is_some());
    }

    #[test]
    fn fork_creates_new_branch() {
        let mut collector = make_collector();
        let root = collector.session_trace().root.clone();

        let fork_id = collector.fork_from(root.clone()).unwrap();
        assert!(!fork_id.is_empty());
        assert_ne!(collector.active_leaf(), &root);
    }

    #[test]
    fn snapshot_lookup_walks_parent_chain() {
        let mut collector = make_collector();
        let root_id = collector.session_trace().root.clone();

        let snap = ContextSnapshot {
            messages: Vec::new(),
            compressed_summary: None,
            token_count: 42,
        };
        collector.attach_snapshot(&root_id, snap.clone()).unwrap();

        // Create a child span
        let handle = collector.begin_span(
            OperationKind::ToolExecution {
                tool_name: "x".to_owned(),
            },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );
        let child_id = handle.node_id.clone();
        collector.end_span(
            handle,
            SpanResult::SkillResult {
                output: String::new(),
            },
        );

        // Snapshot lookup from the child should find the root's snapshot
        let found = collector.get_snapshot_at(child_id).unwrap();
        assert_eq!(found.token_count, 42);
    }
}
