//! The actor's two doors onto unapplied asks: the prelude every other door
//! runs before its own append, and the Resume door an answered dead ask
//! arrives through.

use baybo_model::TurnId;
use baybo_model::human_ask::AskId;
use baybo_store::HumanAskRow;
use baybo_turn::TurnInput;
use tracing::warn;

use super::AgentActor;

impl AgentActor {
    /// Settle this session's unapplied asks under a `SettleOnly` Resume turn,
    /// so the result of each lands adjacent to the row that raised it. A
    /// no-op that opens no turn when there is nothing to settle. Every door
    /// calls this before its own append.
    pub(super) async fn settle_unapplied_asks(&mut self) -> anyhow::Result<()> {
        let session_id = self.durable.session.id.clone();
        let rows = self.volatile.agent_loop.unapplied_asks(&session_id).await?;
        let Some(head) = rows.into_iter().next() else {
            return Ok(());
        };
        let Some(turn_input) = self.resume_input(&head).await? else {
            return Ok(());
        };
        let tokens = baybo_tools::TurnTokens {
            turn: self.volatile.actor_token.child_token(),
            actor: self.volatile.actor_token.clone(),
        };
        let response_tx = self.volatile.response_tx.clone();
        self.volatile
            .agent_loop
            .run_settlement_turn(
                &mut self.durable.session,
                turn_input,
                &self.volatile.turn_lifecycle,
                &self.volatile.span_recorder,
                Some(response_tx),
                tokens,
            )
            .await?;
        Ok(())
    }

    /// A human answered `ask_id` while no tool future waited on it: run the
    /// Resume turn that applies the answer and carries on from it. Nothing
    /// to do when a door's prelude (or a duplicate route) got there first.
    pub(super) async fn handle_resume_ask(&mut self, ask_id: &AskId) -> anyhow::Result<()> {
        let Some(row) = self.volatile.agent_loop.ask(ask_id).await? else {
            warn!(%ask_id, "resume for an ask the ledger does not have");
            return Ok(());
        };
        if row.applied_at.is_some() || row.resolved_at.is_none() {
            return Ok(());
        }
        let Some(turn_input) = self.resume_input(&row).await? else {
            return Ok(());
        };
        let response_tx = self.volatile.response_tx.clone();
        let response = self
            .run_agent_loop(turn_input, None, Some(response_tx), None, None, None)
            .await?;
        self.send_user_reply(response).await;
        Ok(())
    }

    /// The Resume turn payload for `row`: the turn that raised it is the
    /// session's newest turn created no later than the ask. `None` (logged)
    /// when the session has no turns at all, which no raised ask can have.
    async fn resume_input(&self, row: &HumanAskRow) -> anyhow::Result<Option<TurnInput>> {
        let Some(resumed_turn_id) = self.raising_turn(row).await? else {
            warn!(
                session_id = %row.session_id,
                ask_id = %row.ask_id,
                "an unapplied ask with no turn on its session; leaving it"
            );
            return Ok(None);
        };
        Ok(Some(TurnInput::Resume {
            resumed_turn_id,
            ask_id: row.ask_id.clone(),
            tool_use_id: row.tool_call_id.clone(),
        }))
    }

    async fn raising_turn(&self, row: &HumanAskRow) -> anyhow::Result<Option<TurnId>> {
        let turns = self
            .volatile
            .turn_lifecycle
            .list_by_session(&row.session_id, None)
            .await?;
        Ok(turns
            .iter()
            .find(|turn| turn.created_at <= row.created_at)
            .or_else(|| turns.first())
            .map(|turn| turn.id))
    }
}
