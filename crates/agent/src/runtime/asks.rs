//! The agent's port onto the human-ask ledger: the one verb a turn needs, not
//! the manager. Implemented over [`HumanAskManager`].

use async_trait::async_trait;
use baybo_human_ask::HumanAskManager;
use baybo_model::SessionId;
use tracing::warn;

/// What a turn may do to its session's asks.
#[async_trait]
pub trait TurnAsks: Send + Sync {
    /// Claim `Abandoned` for the unapplied asks a cancelled call raised, and
    /// stamp them applied, right before the loop appends that call's result.
    /// The loop appends whatever this finds; a store failure is only logged,
    /// because a dangling `tool_use` would wedge the session's next LLM call.
    async fn abandon_for_tool_call(&self, session: &SessionId, tool_use_id: &str);
}

#[async_trait]
impl TurnAsks for HumanAskManager {
    async fn abandon_for_tool_call(&self, session: &SessionId, tool_use_id: &str) {
        if let Err(e) = HumanAskManager::abandon_for_tool_call(self, session, tool_use_id).await {
            warn!(
                session_id = %session,
                tool_use_id,
                error = %e,
                "could not abandon the asks of a cancelled tool call; appending its result"
            );
        }
    }
}
