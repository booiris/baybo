//! The actor's two doors onto unapplied asks: the prelude every other door
//! runs before its own append, and the Resume door an answered dead ask
//! arrives through.

use super::{AgentActor, VolatileResources};
use crate::runtime::agent_loop::settled_transcript::SettledTranscript;
use baybo_model::human_ask::AskId;

impl VolatileResources {
    pub(super) async fn settle_unapplied_asks(
        &mut self,
        session: &mut baybo_model::Session,
    ) -> anyhow::Result<SettledTranscript<'_>> {
        let (tokens, _release) = self.turn_tokens(&session.id);
        let response_tx = self.response_tx.clone();
        self.agent_loop
            .settle_before_append(
                session,
                &self.turn_lifecycle,
                &self.span_recorder,
                Some(response_tx),
                tokens,
            )
            .await
    }
}

impl AgentActor {
    /// A human answered `ask_id` while no tool future waited on it: run the
    /// Resume turn that applies the answer and carries on from it. Nothing
    /// to do when a door's prelude (or a duplicate route) got there first,
    /// or while the same turn still waits on another answer — the last one
    /// resumes it, with every answer applied.
    pub(super) async fn handle_resume_ask(&mut self, ask_id: &AskId) -> anyhow::Result<()> {
        let Some(row) = self.volatile.agent_loop.resume_ready(ask_id).await? else {
            return Ok(());
        };
        let response_tx = self.volatile.response_tx.clone();
        let response = self
            .run_agent_loop(
                baybo_turn::TurnInput::Resume {
                    resumed_turn_id: row.turn_id,
                },
                None,
                Some(response_tx),
                None,
                None,
            )
            .await?;
        self.send_user_reply(response).await;
        Ok(())
    }
}
