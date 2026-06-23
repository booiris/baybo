use baybo_cron::CronTriggerEvent;
use baybo_model::{TriggerSource, User};
use tracing::{debug, warn};

use crate::actor::AgentMessage;

use super::Router;

impl Router {
    /// Handle a cron trigger by minting a fresh session and routing a
    /// `CronTrigger` message into a one-shot actor.
    ///
    /// Each fire creates an isolated session so the trigger sees a
    /// clean transcript and a fresh `SessionState` (no leaked
    /// `approved_resources` or compression state from prior fires).
    /// Continuity across fires belongs to memory +
    /// skill loading, not to a shared mutable transcript.
    ///
    /// The spawned actor is intentionally NOT registered with the
    /// supervisor: each cron session is one-shot and has no follow-up
    /// traffic, so registering would just accumulate dangling actor
    /// handles in the supervisor's map. We send `CronTrigger` followed
    /// by `ActorStop`; the priority mailbox serves the trigger first
    /// (`CronTrigger` outranks the lowest-priority `ActorStop`), so the
    /// actor processes the fire then exits on `ActorStop`, and its mailbox
    /// closes when this function returns and drops the sender.
    pub(super) async fn handle_cron_trigger(
        &mut self,
        event: CronTriggerEvent,
    ) -> anyhow::Result<()> {
        let user = User {
            id: event.user_id.clone(),
            name: None,
            channel: event.channel.clone(),
        };

        let session = self
            .session_manager
            .create_session_with_trigger(
                user,
                event.channel.clone(),
                TriggerSource::Cron {
                    cron_job_id: event.job_id.clone(),
                },
            )
            .await?;
        let session_id = session.id.clone();

        debug!(
            session_id = %session_id,
            job_id = %event.job_id,
            "routing cron trigger to fresh session"
        );

        let response_tx = self.supervisor.response_tx().clone();
        let (sender, _actor_token) =
            self.spawn_oneshot_actor(session, None, response_tx, &self.actor_parent_token);

        let trigger_msg = AgentMessage::CronTrigger {
            job_id: event.job_id.clone(),
            prompt: event.prompt,
        };
        if let Err(e) = sender.send(trigger_msg).await {
            warn!(session_id = %session_id, error = %e, "failed to deliver cron trigger");
            return Ok(());
        }
        if let Err(e) = sender.send(AgentMessage::ActorStop).await {
            warn!(
                session_id = %session_id,
                error = %e,
                "failed to deliver post-trigger shutdown; actor will still exit when sender drops",
            );
        }
        Ok(())
    }
}
