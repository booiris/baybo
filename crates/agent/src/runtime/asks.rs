//! The agent loop's port onto the human-ask ledger: the verbs a turn needs,
//! not the manager. Implemented over [`HumanAskManager`].

use async_trait::async_trait;
use baybo_human_ask::{HumanAskManager, ToolCallAbandon};
use baybo_model::SessionId;
use tracing::warn;

/// What a turn may do to its session's asks.
#[async_trait]
pub trait TurnAsks: Send + Sync {
    /// Claim `Abandoned` for the unapplied asks a cancelled call raised, right
    /// before the loop appends that call's result. A store failure answers
    /// [`ToolCallAbandon::NoAsk`]: the result is appended anyway, because a
    /// dangling `tool_use` would wedge the session's next LLM call.
    async fn abandon_for_tool_call(
        &self,
        session: &SessionId,
        tool_use_id: &str,
    ) -> ToolCallAbandon;
}

#[async_trait]
impl TurnAsks for HumanAskManager {
    async fn abandon_for_tool_call(
        &self,
        session: &SessionId,
        tool_use_id: &str,
    ) -> ToolCallAbandon {
        match HumanAskManager::abandon_for_tool_call(self, session, tool_use_id).await {
            Ok(claim) => claim,
            Err(e) => {
                warn!(
                    session_id = %session,
                    tool_use_id,
                    error = %e,
                    "could not abandon the asks of a cancelled tool call; appending its result"
                );
                ToolCallAbandon::NoAsk
            }
        }
    }
}
