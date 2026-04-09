//! Fork and rollback helper logic.

#[cfg(test)]
use chrono::Utc;
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
use crate::tree::{create_root_node, set_active_leaf};
#[cfg(test)]
use crate::{ForkRecord, SessionTrace, TraceNodeId};

#[cfg(test)]
pub(crate) fn fork_from(
    trace: &mut SessionTrace,
    from_node: TraceNodeId,
    reason: &str,
) -> crate::Result<String> {
    // Verify the source node exists.
    if !trace.nodes.contains_key(&from_node) {
        return Err(crate::TraceError::NotFound(format!(
            "trace node {from_node}"
        )));
    }

    // Create a new node to serve as the fork root.
    let (fork_root_id, mut fork_root_node) = create_root_node(&trace.session_id);
    fork_root_node.parent = Some(from_node.clone());

    // Register the fork root as a child of from_node.
    if let Some(parent) = trace.nodes.get_mut(&from_node) {
        parent.children.push(fork_root_id.clone());
    }

    trace.nodes.insert(fork_root_id.clone(), fork_root_node);

    let fork_id = Uuid::new_v4().to_string();
    let record = ForkRecord {
        id: fork_id.clone(),
        from_node,
        fork_root: fork_root_id.clone(),
        reason: reason.to_owned(),
        created_at: Utc::now(),
    };
    trace.forks.push(record);

    set_active_leaf(trace, fork_root_id);

    Ok(fork_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::create_root_node;
    use std::collections::HashMap;

    fn make_trace() -> SessionTrace {
        let (root_id, root_node) = create_root_node("test-session");
        let mut nodes = HashMap::new();
        nodes.insert(root_id.clone(), root_node);
        SessionTrace {
            session_id: "test-session".to_owned(),
            root: root_id.clone(),
            nodes,
            forks: Vec::new(),
            active_leaf: root_id,
        }
    }

    #[test]
    fn fork_creates_branch() {
        let mut trace = make_trace();
        let root_id = trace.root.clone();

        let fork_id = fork_from(&mut trace, root_id.clone(), "retry").unwrap();
        assert!(!fork_id.is_empty());
        assert_eq!(trace.forks.len(), 1);
        assert_eq!(trace.forks[0].from_node, root_id);

        // active_leaf should have moved to the fork root
        assert_ne!(trace.active_leaf, root_id);
        assert_eq!(trace.active_leaf, trace.forks[0].fork_root);
    }

    #[test]
    fn fork_from_missing_node_fails() {
        let mut trace = make_trace();
        let result = fork_from(&mut trace, "nonexistent".to_owned(), "oops");
        assert!(result.is_err());
    }
}
