//! Snapshot policy and lookup.

use aura_context::ContextSnapshot;

use crate::{SessionTrace, TraceNodeId};

/// Walk from `node_id` up the parent chain and return the first
/// `ContextSnapshot` found. Returns `NotFound` if no ancestor has a snapshot.
pub fn find_nearest_snapshot(
    trace: &SessionTrace,
    node_id: &TraceNodeId,
) -> aura_core::Result<ContextSnapshot> {
    let mut current = Some(node_id.clone());
    while let Some(ref id) = current {
        let node = trace
            .nodes
            .get(id)
            .ok_or_else(|| aura_core::AuraError::NotFound(format!("trace node {id}")))?;
        if let Some(ref snap) = node.context_snapshot {
            return Ok(snap.clone());
        }
        current = node.parent.clone();
    }
    Err(aura_core::AuraError::NotFound(format!(
        "no context snapshot found in ancestor chain of node {node_id}"
    )))
}

/// Determine whether a snapshot should be taken based on the number of spans
/// created since the last snapshot.
pub fn should_snapshot(spans_since_last: usize, interval: usize) -> bool {
    interval > 0 && spans_since_last >= interval
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_snapshot_at_interval() {
        assert!(!should_snapshot(0, 5));
        assert!(!should_snapshot(4, 5));
        assert!(should_snapshot(5, 5));
        assert!(should_snapshot(10, 5));
    }

    #[test]
    fn should_snapshot_disabled_when_zero() {
        assert!(!should_snapshot(100, 0));
    }
}
