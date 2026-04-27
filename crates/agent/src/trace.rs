use std::collections::HashMap;
use std::sync::Arc;

use aura_context::ContextSnapshot;
use aura_job::OperationKind;
use aura_storage::TraceStore;
use aura_trace::tree::{attach_child, create_root_node, set_active_leaf};
use aura_trace::{
    ExecutionProvenance, SessionTrace, SpanHandle, SpanInput, SpanResult, SpanRole, TraceError,
    TraceNodeId,
};
use uuid::Uuid;

/// Collects trace spans for a single session and persists them via a `TraceStore`.
pub struct TraceCollector {
    session_trace: SessionTrace,
    store: Arc<dyn TraceStore>,
    auto_snapshot: bool,
    snapshot_interval: usize,
    spans_since_snapshot: usize,
    /// ReAct-iteration grouping. While `Some`, every node created via
    /// `begin_span` inherits this `span_id` and `span_index` so callers
    /// can later reconstruct "which LLM call + which tool calls belong
    /// to the same iteration". Set/cleared by `open_iteration` /
    /// `close_iteration`.
    current_span: Option<CurrentSpan>,
}

#[derive(Debug, Clone)]
struct CurrentSpan {
    id: String,
    index: u32,
}

type Result<T> = std::result::Result<T, TraceError>;

impl TraceCollector {
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
            active_leaf: root_id,
        };

        Self {
            session_trace,
            store,
            auto_snapshot,
            snapshot_interval,
            spans_since_snapshot: 0,
            current_span: None,
        }
    }

    /// Open a new ReAct-iteration span. Every `begin_span` call between
    /// now and the matching `close_iteration` will tag its node with the
    /// returned span id and `span_index`. Returns the freshly minted span
    /// id so callers can correlate Progress events with the trace.
    pub fn open_iteration(&mut self, span_index: u32) -> String {
        let id = Uuid::new_v4().to_string();
        self.current_span = Some(CurrentSpan {
            id: id.clone(),
            index: span_index,
        });
        id
    }

    pub fn close_iteration(&mut self) {
        self.current_span = None;
    }

    pub fn begin_span(
        &mut self,
        kind: OperationKind,
        job_id: Option<&str>,
        provenance: ExecutionProvenance,
        input: SpanInput,
    ) -> SpanHandle {
        let span_role = role_for_kind(&kind);
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

        if let Some(node) = self.session_trace.nodes.get_mut(&node_id) {
            node.span_role = span_role;
            if let Some(ref open) = self.current_span {
                node.span_id = open.id.clone();
                node.span_index = open.index;
            }
        }

        set_active_leaf(&mut self.session_trace, node_id.clone());
        self.spans_since_snapshot += 1;

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

    pub fn should_auto_snapshot(&self) -> bool {
        self.auto_snapshot
            && aura_trace::snapshot::should_snapshot(
                self.spans_since_snapshot,
                self.snapshot_interval,
            )
    }

    pub fn attach_snapshot(
        &mut self,
        node_id: &TraceNodeId,
        snapshot: ContextSnapshot,
    ) -> Result<()> {
        let node = self
            .session_trace
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| TraceError::NotFound(format!("trace node {node_id}")))?;
        node.context_snapshot = Some(snapshot);
        self.spans_since_snapshot = 0;
        Ok(())
    }

    pub fn active_leaf(&self) -> &TraceNodeId {
        &self.session_trace.active_leaf
    }

    /// Branch the trace tree at `from_node` so subsequent spans land on
    /// a new chain. Used by the rollback path after the caller has
    /// pulled the snapshot via `find_snapshot_at`.
    pub fn fork_from(&mut self, from_node: TraceNodeId) -> Result<TraceNodeId> {
        aura_trace::fork::fork_from(&mut self.session_trace, from_node)
    }

    /// Find the nearest context snapshot at or above the given node.
    ///
    /// Walks the parent chain until a node with an attached `ContextSnapshot`
    /// is found. Used by the rollback mechanism to restore session state.
    pub fn find_snapshot_at(&self, node_id: &TraceNodeId) -> Result<ContextSnapshot> {
        aura_trace::snapshot::find_nearest_snapshot(&self.session_trace, node_id)
    }
}

fn role_for_kind(kind: &OperationKind) -> SpanRole {
    match kind {
        OperationKind::LlmCall { .. } => SpanRole::Llm,
        OperationKind::ToolExecution { .. } => SpanRole::Tool,
        // Skills are surfaced as system-prompt blocks before the loop
        // and don't run in response to an LLM tool call, so they're
        // bookkeeping rather than agent-loop tool dispatch.
        _ => SpanRole::System,
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
    fn open_iteration_groups_subsequent_nodes_under_same_span_id() {
        let mut collector = make_collector();
        let span_id = collector.open_iteration(2);

        let llm = collector.begin_span(
            OperationKind::LlmCall {
                model: "gpt-4".into(),
            },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );
        collector.end_span(
            llm.clone(),
            SpanResult::SkillResult {
                output: String::new(),
            },
        );
        let tool = collector.begin_span(
            OperationKind::ToolExecution {
                tool_name: "x".into(),
            },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );
        collector.end_span(
            tool.clone(),
            SpanResult::SkillResult {
                output: String::new(),
            },
        );

        let trace = collector.session_trace();
        let llm_node = trace.nodes.get(&llm.node_id).unwrap();
        let tool_node = trace.nodes.get(&tool.node_id).unwrap();
        assert_eq!(llm_node.span_id, span_id);
        assert_eq!(tool_node.span_id, span_id);
        assert_eq!(llm_node.span_index, 2);
        assert_eq!(tool_node.span_index, 2);
        assert_eq!(llm_node.span_role, SpanRole::Llm);
        assert_eq!(tool_node.span_role, SpanRole::Tool);
    }

    #[test]
    fn close_iteration_falls_back_to_per_node_span_id() {
        let mut collector = make_collector();
        collector.open_iteration(0);
        let inside = collector.begin_span(
            OperationKind::LlmCall { model: "m".into() },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );
        collector.end_span(
            inside.clone(),
            SpanResult::SkillResult {
                output: String::new(),
            },
        );
        collector.close_iteration();

        let outside = collector.begin_span(
            OperationKind::ToolExecution {
                tool_name: "x".into(),
            },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );
        let outside_id = outside.node_id.clone();
        collector.end_span(
            outside,
            SpanResult::SkillResult {
                output: String::new(),
            },
        );

        let trace = collector.session_trace();
        // Inside the iteration: span_id matches the one open_iteration minted.
        let inside_node = trace.nodes.get(&inside.node_id).unwrap();
        assert_eq!(inside_node.span_index, 0);
        // After close_iteration: each node defaults to its own id (set by
        // attach_child) — i.e. it forms a span of one node.
        let outside_node = trace.nodes.get(&outside_id).unwrap();
        assert_eq!(outside_node.span_id, outside_id);
        assert_eq!(outside_node.span_index, 0);
    }

    #[test]
    fn span_role_derived_from_operation_kind() {
        let mut collector = make_collector();
        collector.open_iteration(0);
        let llm = collector.begin_span(
            OperationKind::LlmCall { model: "m".into() },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );
        let tool = collector.begin_span(
            OperationKind::ToolExecution {
                tool_name: "t".into(),
            },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );
        let skill = collector.begin_span(
            OperationKind::SkillExecution {
                skill_name: "s".into(),
            },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );
        let mem = collector.begin_span(
            OperationKind::MemoryOperation {
                operation: "store".into(),
            },
            None,
            ExecutionProvenance::default(),
            SpanInput::None,
        );

        let trace = collector.session_trace();
        assert_eq!(
            trace.nodes.get(&llm.node_id).unwrap().span_role,
            SpanRole::Llm
        );
        assert_eq!(
            trace.nodes.get(&tool.node_id).unwrap().span_role,
            SpanRole::Tool
        );
        assert_eq!(
            trace.nodes.get(&skill.node_id).unwrap().span_role,
            SpanRole::System
        );
        assert_eq!(
            trace.nodes.get(&mem.node_id).unwrap().span_role,
            SpanRole::System
        );
    }

    #[test]
    fn snapshot_lookup_walks_parent_chain() {
        let mut collector = make_collector();
        let root_id = collector.session_trace().root.clone();

        let snap = ContextSnapshot {
            messages: Vec::new(),
            token_count: 42,
        };
        collector.attach_snapshot(&root_id, snap.clone()).unwrap();

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

        let found = collector.find_snapshot_at(&child_id).unwrap();
        assert_eq!(found.token_count, 42);
    }
}
