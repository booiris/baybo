//! Narrow ports `baybo-human-ask` defines and other crates implement, so it
//! never has to name a channel, the router or the session store.

use async_trait::async_trait;
use baybo_model::SessionId;
use baybo_model::human_ask::ResumeRequest;
use baybo_store::HumanAskRow;

use crate::core::AskEvent;

/// Reaches the router when a human answers an ask whose tool future is gone.
#[async_trait]
pub trait ResumeSink: Send + Sync {
    /// Whether an answer to a dead ask in this session should open a chat
    /// Resume turn. Issue sessions answer `false`: the run's own re-drive
    /// applies the answer under the real run.
    async fn resumes_on_answer(&self, session_id: &SessionId) -> bool;

    /// Ask the router to open a Resume turn for a resolved ask. Fire and
    /// forget: a lost request leaves the row resolved, and the next turn or
    /// boot applies it.
    fn request_resume(&self, req: ResumeRequest);
}

/// "Is a tool future parked on an ask in this session right now?" — the idle
/// reaper's probe, answered from memory.
pub trait LivePendingAsks: Send + Sync {
    fn has_live_pending_ask(&self, session_id: &SessionId) -> bool;
}

/// Told about asks raised and resolved through a verb. Runs on the verb's
/// task after the store write, never from a queue watcher and never on boot
/// reload.
#[async_trait]
pub trait AskObserver: Send + Sync {
    async fn on_event(&self, ask: &HumanAskRow, event: AskEvent);
}
