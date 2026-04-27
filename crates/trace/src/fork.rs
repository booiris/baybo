//! In-session branch helpers.
//!
//! Used by the rollback path to attach a fresh "fork-root" trace node
//! under an arbitrary ancestor so subsequent spans land on a new
//! branch without overwriting the original chain. Cross-session forks
//! (user "branch this conversation") live on
//! `aura_model::Session::parent_link` and never touch this module.

use crate::tree::{create_root_node, set_active_leaf};
use crate::{SessionTrace, TraceNodeId};

/// Attach a new branch under `from_node` and move `active_leaf` to it.
/// Returns the id of the freshly created branch root so callers can
/// thread it through their own bookkeeping (snapshot lookup, cost
/// attribution, etc.).
pub fn fork_from(trace: &mut SessionTrace, from_node: TraceNodeId) -> crate::Result<TraceNodeId> {
    if !trace.nodes.contains_key(&from_node) {
        return Err(crate::TraceError::NotFound(format!(
            "trace node {from_node}"
        )));
    }

    let (fork_root_id, mut fork_root_node) = create_root_node(&trace.session_id);
    fork_root_node.parent = Some(from_node.clone());

    if let Some(parent) = trace.nodes.get_mut(&from_node) {
        parent.children.push(fork_root_id.clone());
    }
    trace.nodes.insert(fork_root_id.clone(), fork_root_node);

    set_active_leaf(trace, fork_root_id.clone());

    Ok(fork_root_id)
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
            active_leaf: root_id,
        }
    }

    #[test]
    fn fork_creates_branch() {
        let mut trace = make_trace();
        let root_id = trace.root.clone();

        let fork_root = fork_from(&mut trace, root_id.clone()).unwrap();
        assert!(!fork_root.is_empty());
        assert_eq!(trace.active_leaf, fork_root);

        let fork_node = trace.nodes.get(&fork_root).unwrap();
        assert_eq!(fork_node.parent.as_ref(), Some(&root_id));
        let parent = trace.nodes.get(&root_id).unwrap();
        assert!(parent.children.contains(&fork_root));
    }

    #[test]
    fn fork_from_missing_node_fails() {
        let mut trace = make_trace();
        let result = fork_from(&mut trace, "nonexistent".to_owned());
        assert!(result.is_err());
    }
}
