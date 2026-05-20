use std::sync::Arc;

use aura_channels::{AgentOutput, IncomingMessage, Message};
use aura_model::{
    BACKGROUND_SUBAGENT_HANDLE_PREFIX, ChannelType, ContentBlock, JobId, Lineage, LineageKind,
    MessageMetadata, PendingSubagentResult, SUBAGENT_CHANNEL_TAG, SessionId, SpanId,
    SubagentExitStatus, SubagentResult, SubagentSpawnRequest, User,
};
use chrono::Utc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::actor::AgentMessage;
use crate::actor::router::Router;
use crate::actor::subagent::await_subagent_terminal;
use crate::actor::supervisor::AgentSupervisor;

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
        // `None` is the synthesized-from-tests shape — those tests
        // are not gated by the fan-out limiter and the release is a
        // no-op. Production spawns always have a root because the
        // tool reserved one before sending the envelope.
        let fan_out_root = request.fan_out_root.clone();
        let parent = match self.session_manager.get(&parent_session_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                let _ = result_tx.send(SubagentResult::failed(format!(
                    "parent session {parent_session_id} not found"
                )));
                self.release_fan_out_slot(&fan_out_root);
                return Ok(());
            }
            Err(e) => {
                let _ = result_tx.send(SubagentResult::failed(format!("load parent session: {e}")));
                self.release_fan_out_slot(&fan_out_root);
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
                self.release_fan_out_slot(&fan_out_root);
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
        // Tier resolution: explicit `llm` > `model_tier` lookup > pool
        // default (handled inside the spawner closure when `None` is
        // passed). The tool already merged the profile's default_tier
        // into `model_tier`, so this is the final step.
        let llm = request.llm.clone().or_else(|| {
            request
                .model_tier
                .and_then(|t| self.llm_pool.resolve_tier(t))
        });
        let system_prompt_override = Some(request.system_prompt.clone());
        let background = request.background;
        let subagent_type = request.subagent_type.clone();
        let task_summary = request.task_summary.clone();

        // Background subagents must outlive the parent's per-job
        // cancel scope — the job that emitted `spawn_subagent` will
        // end as soon as the tool returns the ack, so anchoring the
        // child to that token would tear it down immediately. The
        // process-wide `actor_parent_token` is the right ancestor:
        // process shutdown still cascades, but the parent's
        // per-job/per-turn lifecycle no longer drags the child down.
        let effective_parent_token = if background {
            self.actor_parent_token.clone()
        } else {
            parent_actor_token.clone()
        };

        let (output_tx, output_rx) = mpsc::channel::<AgentOutput>(SUBAGENT_OUTPUT_BUFFER);
        let (mailbox, actor_token) = self.spawn_oneshot_actor(
            child_session,
            llm,
            output_tx,
            &effective_parent_token,
            system_prompt_override,
        );

        if let Err(e) = mailbox
            .send(AgentMessage::SubagentSpawned {
                initial_message: Box::new(incoming),
                parent_job_id,
            })
            .await
        {
            let _ = result_tx.send(SubagentResult::failed(format!("dispatch child input: {e}")));
            self.release_fan_out_slot(&fan_out_root);
            return Ok(());
        }

        if background {
            let handle_id = format!(
                "{}{}",
                BACKGROUND_SUBAGENT_HANDLE_PREFIX,
                uuid::Uuid::new_v4()
            );
            let ack_text = format!(
                "[background subagent dispatched]\n- handle: {handle_id}\n- subagent_type: {subagent_type}\n- child_session: {child_session_id}\n\nThe runtime will surface the subagent's final message as a system reminder prepended to your next user turn."
            );
            let ack = SubagentResult {
                child_session_id: child_session_id.clone(),
                final_content: Some(vec![ContentBlock::Text(ack_text)]),
                status: SubagentExitStatus::Completed,
            };
            let _ = result_tx.send(ack);

            // Pin the parent against the idle reaper for the
            // duration of the background child; the wait task below
            // clears the counter on every terminal path.
            self.supervisor
                .note_background_subagent_started(&parent.id);
            if let Err(e) = self.session_manager.touch(&parent.id).await {
                warn!(
                    parent_session_id = %parent.id,
                    error = %e,
                    "background spawn: failed to touch parent session"
                );
            }

            let job_lifecycle = Arc::clone(&self.job_lifecycle);
            let supervisor = self.supervisor.clone();
            let parent_id_for_task = parent.id.clone();
            let fan_out_root_for_task = fan_out_root.clone();
            let limiter_for_task = Arc::clone(&self.dispatch_limiter);
            tokio::spawn(async move {
                let result = await_subagent_terminal(
                    child_session_id.clone(),
                    output_rx,
                    terminal_rx,
                    mailbox,
                    actor_token,
                    timeout,
                    job_lifecycle,
                )
                .await;
                deliver_background_result(
                    &supervisor,
                    &parent_id_for_task,
                    handle_id,
                    subagent_type,
                    task_summary,
                    result,
                )
                .await;
                // Counter must be cleared AFTER `deliver_background_result`
                // observes the terminal so the reaper cannot tear the
                // parent down in the window between the wait task seeing
                // the terminal event and the mailbox receiving it.
                // Order matters: clear the per-parent counter only
                // AFTER the SubagentFinished message has reached the
                // mailbox, so the reaper can't tear the parent down
                // in the window between terminal-observe and deliver.
                supervisor.note_background_subagent_finished(&parent_id_for_task);
                if let Some(root) = fan_out_root_for_task {
                    limiter_for_task.release(&root);
                }
            });
            return Ok(());
        }

        let job_lifecycle = Arc::clone(&self.job_lifecycle);
        let limiter_for_task = Arc::clone(&self.dispatch_limiter);
        let fan_out_root_for_task = fan_out_root.clone();
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
            if let Some(root) = fan_out_root_for_task {
                limiter_for_task.release(&root);
            }
        });
        Ok(())
    }

    fn release_fan_out_slot(&self, root: &Option<SessionId>) {
        if let Some(id) = root {
            self.dispatch_limiter.release(id);
        }
    }
}

/// Post the background subagent's terminal result to the parent
/// actor's mailbox so the next user turn picks it up as a system
/// reminder.
///
/// If the parent actor is no longer registered (idle-reaped between
/// spawn and finish, or never rehydrated after a crash), `route`
/// returns false. We log a warning; the result is still preserved in
/// the trace tree and the child's session row. Recovery on a fresh
/// hydration is a future improvement (TODO: storage-backed pending
/// buffer so an evicted parent's deliveries survive).
async fn deliver_background_result(
    supervisor: &AgentSupervisor,
    parent_session_id: &SessionId,
    handle_id: String,
    subagent_type: String,
    task_summary: String,
    result: SubagentResult,
) {
    let parts = result.split_for_parent();
    let pending = PendingSubagentResult {
        handle_id: handle_id.clone(),
        subagent_type,
        task_summary,
        child_session_id: result.child_session_id,
        final_text: parts.text,
        images: parts.llm_images,
        status: result.status,
    };
    let delivered = supervisor
        .route(
            parent_session_id,
            AgentMessage::SubagentFinished(Box::new(pending)),
        )
        .await;
    if !delivered {
        warn!(
            parent_session_id = %parent_session_id,
            handle_id = %handle_id,
            "background subagent terminal could not be routed — parent actor not registered; result available in trace/child session only"
        );
    }
}
