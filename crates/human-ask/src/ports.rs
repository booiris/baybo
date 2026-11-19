//! Narrow ports `baybo-human-ask` defines and other crates implement, so it
//! never has to name a channel, the router or the session store.

use async_trait::async_trait;
use baybo_model::SessionId;
use baybo_model::human_ask::ResumeRequest;
use baybo_store::HumanAskRow;

use crate::core::AskEvent;

/// Reaches the router when a human answers an ask whose tool future is gone.
pub trait ResumeSink: Send + Sync {
    /// Tell the router a dead ask was answered. Fire and forget: whether
    /// that opens a Resume turn is the router's call (an issue session's own
    /// run applies the answer instead), and a lost request leaves the row
    /// resolved for the session's next turn or the next boot to apply.
    fn request_resume(&self, req: ResumeRequest);
}

/// What stopping a session's actor would cost its live asks, the ones a tool
/// future is parked on right now. Ordered by cost, so the cost of several
/// asks is the `max` of theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActorStopCost {
    /// No tool future is parked on an ask: the actor stops between turns.
    Free,
    /// Every live ask is resumable. The running turn has to be released
    /// first; each ask then stays pending, and an answer resumes the session
    /// through the Resume door.
    ReleasesTurn,
    /// A live ask is not resumable (a during-execute approval): stopping
    /// would abandon it. Goes away once those approvals move before execute
    /// (`docs/todo/during-execute-approvals.md`).
    AbandonsAsks,
}

/// "What would stopping this session's actor cost?" — the idle reaper's
/// probe, answered from memory.
pub trait LivePendingAsks: Send + Sync {
    fn actor_stop_cost(&self, session_id: &SessionId) -> ActorStopCost;
}

/// Told about asks raised and resolved through a verb. Runs on the verb's
/// task after the store write, never from a queue watcher. Boot reload sends
/// no `Raised`; a reload that abandons a dead non-resumable ask does send
/// `Resolved(Abandoned)`.
#[async_trait]
pub trait AskObserver: Send + Sync {
    async fn on_event(&self, ask: &HumanAskRow, event: AskEvent);
}
