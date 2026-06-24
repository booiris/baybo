//! Capacity gate for `spawn_subagent` dispatch.
//!
//! The fan-out cap is enforced *outside* the depth check: depth bounds
//! the lineage chain length (how deep a single thread of subagents
//! goes), while fan-out bounds the concurrent breadth under one root
//! session (how many parallel siblings + cousins a single LLM can keep
//! alive at once). Both are soft, configurable cost guards.
//!
//! Lives in this leaf crate (alongside the profile registry) so both
//! consumers share one definition without a cycle: `baybo-tools`'
//! `spawn_subagent` builtin holds an `Arc<dyn SubagentDispatchLimiter>`
//! to reserve a slot, and `baybo-agent`'s router releases it on the
//! child's terminal event. `baybo-subagent` depends on neither crate,
//! so neither direction closes a loop.
//!
//! Lifecycle contract:
//!  1. Tool calls `try_reserve(root, cap)`.
//!  2. On `Ok(())`: a slot has been incremented. The caller MUST
//!     eventually call `release(root)` exactly once. Multiple release
//!     paths converge on the supervisor (envelope-send failure,
//!     router-level setup failure, wait task's terminal hook); each
//!     emits one release.
//!  3. On `Err(current_count)`: nothing was incremented. The caller
//!     surfaces `ToolError::SubagentFanoutExceeded`.

use std::sync::Arc;

use baybo_model::SessionId;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

/// Tracks in-flight subagent count per root session and gates new
/// spawns against a per-call cap. Implementations must make
/// `try_reserve` + the matching `release` atomic with respect to
/// each other so two parallel spawns under the same root cannot
/// both observe a count of `cap - 1` and both succeed.
pub trait SubagentDispatchLimiter: Send + Sync {
    /// Atomically: if the in-flight count under `root_session_id`
    /// is `< cap`, increment it and return `Ok(())`. Otherwise leave
    /// the count alone and return `Err(current_count)` so the caller
    /// can surface the overflow with a concrete number.
    ///
    /// `cap == 0` always fails — a deployment configured this way is
    /// effectively disabling subagent dispatch.
    fn try_reserve(&self, root_session_id: &SessionId, cap: u32) -> Result<(), u32>;

    /// Counterpart to a successful [`Self::try_reserve`]. Idempotent
    /// against unknown roots (returns silently) so the eventual
    /// terminal-event hook does not crash if the spawn never reached
    /// dispatch.
    fn release(&self, root_session_id: &SessionId);

    /// Observe the current count for diagnostics. Not used by the
    /// hot path; surfaced for the operator CLI.
    fn in_flight(&self, root_session_id: &SessionId) -> u32;

    /// Snapshot every (root, count) entry for operator-facing views.
    /// Default returns empty so the unbounded stub doesn't need to
    /// fabricate data; the production `FanOutLimiter` overrides this
    /// to walk its DashMap.
    fn snapshot(&self) -> Vec<(SessionId, u32)> {
        Vec::new()
    }
}

/// Stub implementation that never blocks dispatch. Used by tests that
/// only care about the rest of the spawn path. `release` is a no-op
/// and `try_reserve` always returns `Ok(())`.
pub struct UnboundedDispatchLimiter;

impl SubagentDispatchLimiter for UnboundedDispatchLimiter {
    fn try_reserve(&self, _root_session_id: &SessionId, _cap: u32) -> Result<(), u32> {
        Ok(())
    }
    fn release(&self, _root_session_id: &SessionId) {}
    fn in_flight(&self, _root_session_id: &SessionId) -> u32 {
        0
    }
}

/// Convenience constructor for tests / non-production wiring.
pub fn unbounded_limiter() -> Arc<dyn SubagentDispatchLimiter> {
    Arc::new(UnboundedDispatchLimiter)
}

/// Process-wide counting limiter. Tracks in-flight subagents per
/// root session in a sharded DashMap. Cloned via `Arc<FanOutLimiter>`
/// so the `spawn_subagent` tool (which reserves) and the router's
/// wait task (which releases on terminal) share state without the
/// counter being threaded through the agent supervisor.
///
/// The DashMap entry API gives us a check-and-increment that's
/// atomic within a single shard, which is the granularity required:
/// two concurrent dispatches under the same root contend on the
/// same shard and cannot both observe a count of `cap - 1`.
#[derive(Default)]
pub struct FanOutLimiter {
    counts: DashMap<SessionId, u32>,
}

impl FanOutLimiter {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SubagentDispatchLimiter for FanOutLimiter {
    fn try_reserve(&self, root_session_id: &SessionId, cap: u32) -> Result<(), u32> {
        if cap == 0 {
            return Err(0);
        }
        match self.counts.entry(root_session_id.clone()) {
            Entry::Occupied(mut slot) => {
                let current = *slot.get();
                if current >= cap {
                    Err(current)
                } else {
                    *slot.get_mut() = current.saturating_add(1);
                    Ok(())
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(1);
                Ok(())
            }
        }
    }

    fn release(&self, root_session_id: &SessionId) {
        // Decrement-or-remove must stay atomic within the shard: holding
        // the `Entry` guard across the whole branch stops a concurrent
        // `try_reserve` from re-incrementing a zero entry that we then
        // delete (which would leak the reservation and let the cap be
        // exceeded). Dropping a `get_mut` guard before a separate
        // `remove` reopens exactly that window.
        if let Entry::Occupied(mut slot) = self.counts.entry(root_session_id.clone()) {
            let new = slot.get().saturating_sub(1);
            if new == 0 {
                slot.remove();
            } else {
                *slot.get_mut() = new;
            }
        }
    }

    fn in_flight(&self, root_session_id: &SessionId) -> u32 {
        self.counts.get(root_session_id).map(|v| *v).unwrap_or(0)
    }

    fn snapshot(&self) -> Vec<(SessionId, u32)> {
        self.counts
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbounded_never_blocks() {
        let lim = UnboundedDispatchLimiter;
        let root = SessionId::from("root");
        assert!(lim.try_reserve(&root, 1).is_ok());
        assert!(lim.try_reserve(&root, 1).is_ok());
        assert!(lim.try_reserve(&root, 0).is_ok());
        assert_eq!(lim.in_flight(&root), 0);
        lim.release(&root);
    }

    #[test]
    fn fan_out_limiter_caps_at_configured_value() {
        let lim = FanOutLimiter::new();
        let root = SessionId::from("root");
        for _ in 0..3 {
            assert!(lim.try_reserve(&root, 3).is_ok());
        }
        assert_eq!(lim.try_reserve(&root, 3), Err(3));
        assert_eq!(lim.in_flight(&root), 3);
    }

    #[test]
    fn fan_out_limiter_release_clears_entry_at_zero() {
        let lim = FanOutLimiter::new();
        let root = SessionId::from("root");
        assert!(lim.try_reserve(&root, 5).is_ok());
        assert!(lim.try_reserve(&root, 5).is_ok());
        lim.release(&root);
        lim.release(&root);
        assert_eq!(lim.in_flight(&root), 0);
        assert!(lim.snapshot().is_empty());
    }

    #[test]
    fn fan_out_limiter_release_unknown_root_is_noop() {
        let lim = FanOutLimiter::new();
        lim.release(&SessionId::from("ghost"));
        assert!(lim.snapshot().is_empty());
    }

    #[test]
    fn fan_out_limiter_cap_zero_always_denies() {
        let lim = FanOutLimiter::new();
        assert_eq!(lim.try_reserve(&SessionId::from("root"), 0), Err(0));
    }

    #[test]
    fn fan_out_limiter_distinct_roots_have_independent_counts() {
        let lim = FanOutLimiter::new();
        let a = SessionId::from("a");
        let b = SessionId::from("b");
        assert!(lim.try_reserve(&a, 1).is_ok());
        assert!(lim.try_reserve(&b, 1).is_ok());
        assert_eq!(lim.try_reserve(&a, 1), Err(1));
        assert_eq!(lim.in_flight(&a), 1);
        assert_eq!(lim.in_flight(&b), 1);
    }

    #[test]
    fn fan_out_limiter_snapshot_lists_all_active_roots() {
        let lim = FanOutLimiter::new();
        let a = SessionId::from("a");
        let b = SessionId::from("b");
        let _ = lim.try_reserve(&a, 5);
        let _ = lim.try_reserve(&a, 5);
        let _ = lim.try_reserve(&b, 5);
        let mut snap = lim.snapshot();
        snap.sort_by(|x, y| x.0.as_ref().cmp(y.0.as_ref()));
        assert_eq!(snap, vec![(a, 2), (b, 1)]);
    }
}
