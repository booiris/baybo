//! How many tool calls of a turn are executing right now.
//!
//! The idle reaper may end a turn that is only waiting on resumable asks.
//! The ask queue can say how many asks wait; only the loop can say whether
//! every running call is one of those waiters, or whether a sibling (a
//! foreground subagent, a slow tool, a call whose approval was just answered)
//! is still doing work the release would cut. The supervisor compares the
//! two counts.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A turn's count of running tool calls, shared by the loop that runs them
/// and the supervisor that reads it. A call counts from the moment it starts
/// executing (holds its pool permit) until it returns or is dropped; one
/// still queued behind an exclusive call is not running.
#[derive(Debug, Clone, Default)]
pub struct RunningCalls(Arc<AtomicUsize>);

impl RunningCalls {
    /// Count one call as running until the returned guard drops.
    pub(crate) fn start(&self) -> RunningCall {
        self.0.fetch_add(1, Ordering::AcqRel);
        RunningCall(Arc::clone(&self.0))
    }

    pub(crate) fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

/// RAII half of [`RunningCalls::start`]; decrements on drop, including when
/// the call's future is dropped mid-run.
#[derive(Debug)]
pub(crate) struct RunningCall(Arc<AtomicUsize>);

impl Drop for RunningCall {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_counts_only_while_its_guard_lives() {
        let running = RunningCalls::default();
        let first = running.start();
        let second = running.clone().start();
        assert_eq!(running.count(), 2);
        drop(first);
        assert_eq!(running.count(), 1);
        drop(second);
        assert_eq!(running.count(), 0);
    }
}
