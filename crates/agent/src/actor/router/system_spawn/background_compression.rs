use aura_model::{BackgroundCompressionPayload, JobId, SessionId};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::actor::AgentMessage;
use crate::actor::router::Router;

impl Router {
    pub(super) async fn handle_background_compression_spawn(
        &mut self,
        parent_session_id: SessionId,
        parent_job_id: JobId,
        parent_actor_token: CancellationToken,
        payload: BackgroundCompressionPayload,
    ) -> anyhow::Result<()> {
        let parent = self
            .session_manager
            .get(&parent_session_id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("parent session {parent_session_id} not found for summary spawn")
            })?;

        let maint = self
            .session_manager
            .create_maintenance_session(
                &parent,
                parent_job_id,
                aura_model::SystemReason::BackgroundCompression,
            )
            .await?;
        let maint_session_id = maint.id.clone();

        debug!(
            parent_session_id = %parent_session_id,
            maint_session_id = %maint_session_id,
            "routing system-spawn request to fresh maintenance session"
        );

        // Maintenance spawns always run on `default-llm`. The parent
        // session itself has no `last_llm` to inherit — LLM pinning
        // is subagent-only (`SubagentSpawnRequest.llm`), and
        // subagents do not currently trigger background compression
        // on their own transcripts. If that changes, plumb the
        // parent's effective LLM through `SystemSpawnRequest`.
        let response_tx = self.supervisor.response_tx().clone();
        let (mailbox, _actor_token) =
            self.spawn_oneshot_actor(maint, None, response_tx, &parent_actor_token, None);

        if let Err(e) = mailbox
            .send(AgentMessage::BackgroundCompression(payload))
            .await
        {
            warn!(
                parent_session_id = %parent_session_id,
                maint_session_id = %maint_session_id,
                error = %e,
                "failed to deliver BackgroundCompression to maintenance actor mailbox"
            );
        }
        Ok(())
    }
}
