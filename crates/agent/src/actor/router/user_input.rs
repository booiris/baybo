use aura_channels::IncomingMessage;
use aura_model::SessionId;
use tracing::{debug, warn};

use crate::actor::AgentMessage;

use super::Router;

impl Router {
    pub(super) async fn handle_incoming(
        &mut self,
        mut incoming: IncomingMessage,
    ) -> anyhow::Result<()> {
        let span = tracing::info_span!(
            "handle_incoming",
            session_id = %incoming.message.session_id,
            message_id = %incoming.message.id,
            channel = %incoming.message.channel,
        );
        let _guard = span.enter();

        let session_id = incoming.message.session_id.clone();
        let user = incoming.message.sender.clone();
        let channel = incoming.message.channel.clone();

        debug!(session_id = %session_id, user_id = %user.id, "routing message");

        // User-level rate limiting
        if !self.rate_limiter.check(&user.id) {
            warn!(
                user_id = %user.id,
                session_id = %session_id,
                "user rate-limited"
            );
            anyhow::bail!("rate limit exceeded for user '{}'", user.id);
        }

        // In-memory budget gate: same call agent_loop makes before
        // each LLM call, fired here too so an over-cap user never even
        // gets an actor spun up.
        self.cost_manager.check().map_err(|e| {
            warn!(
                user_id = %user.id,
                session_id = %session_id,
                error = %e,
                "cost manager rejected request"
            );
            anyhow::anyhow!(e)
        })?;

        // Get or create session
        let typed_session_id = SessionId::from(session_id.as_str());
        let mut session = self
            .session_manager
            .get_or_create(&typed_session_id, user, channel)
            .await?;

        // Sanitize input through the security gateway before routing.
        if let Err(e) = self
            .security_gateway
            .sanitize_input(&mut incoming.message, &mut session)
            .await
        {
            warn!(
                session_id = %session_id,
                error = %e,
                "security gateway blocked or modified incoming message"
            );
            return Err(e.into());
        }

        // Update last active time
        self.session_manager.touch(&typed_session_id).await?;

        // Route to the session's actor, lazily spawning one if the
        // session has no live actor yet. User-channel actors are not
        // allowed to override the LLM at spawn time: the spawner reads
        // `session.state.last_llm` (set by admin-side session creation)
        // for hydration, or falls back to the pool default. Mid-session
        // swaps are not exposed to user-channel callers —
        // `SubagentSpawnRequest` is the only path that pins a
        // non-default model.
        let response_tx = self.supervisor.response_tx().clone();
        let parent_token = self.actor_parent_token.clone();
        let actor_spawner = self.actor_spawner.as_ref();
        let routed = self
            .supervisor
            .route_or_spawn(
                &session_id,
                AgentMessage::UserInput(Box::new(incoming)),
                || {
                    let actor_token = parent_token.child_token();
                    actor_spawner(session, None, response_tx, actor_token, None)
                },
            )
            .await;
        if !routed {
            warn!(session_id = %session_id, "failed to route user input to actor");
        }

        Ok(())
    }
}
