//! Tree operations and active_leaf maintenance.

use aura_job::OperationKind;
use chrono::Utc;
use uuid::Uuid;

use crate::{ExecutionProvenance, SessionTrace, SpanInput, TraceNode, TraceNodeId, TraceSpan};

/// Create a new root node for a fresh session trace.
pub fn create_root_node(session_id: &str) -> (TraceNodeId, TraceNode) {
    let id = Uuid::new_v4().to_string();
    let node = TraceNode {
        id: id.clone(),
        parent: None,
        children: Vec::new(),
        span: TraceSpan {
            kind: OperationKind::UserMessageHandling {
                session_id: session_id.to_owned(),
            },
            job_id: None,
            provenance: ExecutionProvenance::default(),
            input: SpanInput::None,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            result: None,
        },
        context_snapshot: None,
    };
    (id, node)
}

/// Attach a new child node under the given parent in the trace tree.
///
/// Returns the id of the newly created node.
pub fn attach_child(
    trace: &mut SessionTrace,
    parent_id: &TraceNodeId,
    kind: OperationKind,
    job_id: Option<&str>,
    provenance: ExecutionProvenance,
    input: SpanInput,
) -> crate::Result<TraceNodeId> {
    let child_id = Uuid::new_v4().to_string();
    let child = TraceNode {
        id: child_id.clone(),
        parent: Some(parent_id.clone()),
        children: Vec::new(),
        span: TraceSpan {
            kind,
            job_id: job_id.map(str::to_owned),
            provenance,
            input,
            started_at: Utc::now(),
            ended_at: None,
            result: None,
        },
        context_snapshot: None,
    };

    // Insert the child node first, then update the parent's children list.
    trace.nodes.insert(child_id.clone(), child);

    let parent = trace
        .nodes
        .get_mut(parent_id)
        .ok_or_else(|| crate::TraceError::NotFound(format!("parent node {parent_id}")))?;
    parent.children.push(child_id.clone());

    Ok(child_id)
}

/// Update `active_leaf` to the given node id.
pub fn set_active_leaf(trace: &mut SessionTrace, node_id: TraceNodeId) {
    trace.active_leaf = node_id;
}

pub fn ancestor_chain(trace: &SessionTrace, node_id: &TraceNodeId) -> Vec<TraceNodeId> {
    let mut chain = Vec::new();
    let mut current = Some(node_id.clone());
    while let Some(ref id) = current {
        chain.push(id.clone());
        current = trace.nodes.get(id).and_then(|n| n.parent.clone());
    }
    chain
}
