use std::sync::Arc;

use aura_channels::{AgentOutput, IncomingMessage, Message};
use aura_model::{
    ChannelType, ContentBlock, JobId, Lineage, LineageKind, MessageMetadata, SUBAGENT_CHANNEL_TAG,
    SessionId, SpanId, SubagentResult, SubagentSpawnRequest, User,
};
use chrono::Utc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::actor::AgentMessage;
use crate::actor::router::Router;
use crate::actor::subagent::await_subagent_terminal;

/// `output_tx` buffer for a subagent's actor. Intentionally smaller than
/// the operator-configured channel size for top-level actors — a child
/// session only emits its final `AgentOutput::Message` (deltas are not
/// forwarded back through this channel), so 64 is overkill but matches
/// the wait routine's earlier sizing.
const SUBAGENT_OUTPUT_BUFFER: usize = 64;

impl Router {
    pub(super) async fn handle_subagent_spawn(
        &mut self,
        parent_session_id: SessionId,
        parent_job_id: JobId,
        parent_span_id: SpanId,
        parent_actor_token: CancellationToken,
        request: SubagentSpawnRequest,
        result_tx: oneshot::Sender<SubagentResult>,
    ) -> anyhow::Result<()> {
        let parent = match self.session_manager.get(&parent_session_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                let _ = result_tx.send(SubagentResult::failed(format!(
                    "parent session {parent_session_id} not found"
                )));
                return Ok(());
            }
            Err(e) => {
                let _ = result_tx.send(SubagentResult::failed(format!("load parent session: {e}")));
                return Ok(());
            }
        };

        let child_channel = ChannelType::from(SUBAGENT_CHANNEL_TAG);
        let child_user = User {
            id: parent.user.id.clone(),
            name: parent.user.name.clone(),
            channel: child_channel.clone(),
        };
        let lineage = Lineage {
            parent_session_id,
            parent_job_id,
            parent_span_id: Some(parent_span_id),
            kind: LineageKind::Subagent,
        };
        let child_session = match self
            .session_manager
            .create_spawned_session(child_user, child_channel, &parent, lineage)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let _ =
                    result_tx.send(SubagentResult::failed(format!("create child session: {e}")));
                return Ok(());
            }
        };

        // Subscribe to terminal events BEFORE dispatch so a child whose
        // actor exits synchronously cannot terminate between
        // dispatch and the receiver being open.
        let terminal_rx = self.job_lifecycle.subscribe_terminal_events();

        let now = Utc::now();
        let incoming = IncomingMessage {
            message: Message {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: child_session.id.clone(),
                channel: child_session.channel.clone(),
                sender: child_session.user.clone(),
                content: vec![ContentBlock::Text(request.initial_prompt())],
                timestamp: now,
                reply_to: None,
                metadata: MessageMetadata::default(),
            },
            platform_msg_id: String::new(),
        };
        let child_session_id = child_session.id.clone();
        let timeout = request.timeout;
        let llm = request.llm.clone();

        let (output_tx, output_rx) = mpsc::channel::<AgentOutput>(SUBAGENT_OUTPUT_BUFFER);
        let (mailbox, actor_token) =
            self.spawn_oneshot_actor(child_session, llm, output_tx, &parent_actor_token);

        if let Err(e) = mailbox
            .send(AgentMessage::SubagentSpawned {
                initial_message: Box::new(incoming),
                parent_job_id,
            })
            .await
        {
            let _ = result_tx.send(SubagentResult::failed(format!("dispatch child input: {e}")));
            return Ok(());
        }

        let job_lifecycle = Arc::clone(&self.job_lifecycle);
        tokio::spawn(async move {
            let result = await_subagent_terminal(
                child_session_id,
                output_rx,
                terminal_rx,
                mailbox,
                actor_token,
                timeout,
                job_lifecycle,
            )
            .await;
            let _ = result_tx.send(result);
        });
        Ok(())
    }
}
