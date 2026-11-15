//! Narrow ports `baybo-human-ask` defines and other crates implement, so it
//! never has to name a channel, the router or the session store.

use async_trait::async_trait;
use baybo_model::SessionId;
use baybo_store::HumanAskRow;

use crate::core::AskEvent;

/// "Is a tool future parked on an ask in this session right now?" — the idle
/// reaper's probe, answered from memory. An actor the reaper stopped would
/// orphan the ask, and nothing could apply an answer to it.
pub trait LivePendingAsks: Send + Sync {
    fn has_live_pending_ask(&self, session_id: &SessionId) -> bool;
}

/// Told about asks raised and resolved through a verb. Runs on the verb's
/// task after the store write, never from a queue watcher.
#[async_trait]
pub trait AskObserver: Send + Sync {
    async fn on_event(&self, ask: &HumanAskRow, event: AskEvent);
}
