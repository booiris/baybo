use std::sync::Arc;

use aura_channels::{AgentOutput, COMPACT_COMMAND, IncomingMessage, NoticeLevel, OutgoingMessage};
use aura_job::JobInput;
use aura_model::{ContentBlock, Session};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::agent_loop::AgentLoop;
use crate::job::JobLifecycle;
use crate::trace::SpanRecorder;

/// Messages that can be sent to an AgentActor.
#[derive(Debug, Clone)]
pub enum AgentMessage {
    /// A user sent a message.
    UserInput(Box<IncomingMessage>),
    /// A cron job fired.
    CronTrigger { job_id: String, prompt: String },
    /// A subagent was spawned. Carries the initial prompt assembled by
    /// `LocalSubagentRuntime` and the parent's `JobId` for lineage.
    /// The child actor runs `agent_loop.run` with `JobInput::Spawned`,
    /// which `JobKind::Spawned.allowed_for(*) == true` lets through
    /// regardless of the child session's root trigger — which it must,
    /// because subagents inherit the parent's trigger (cron / system)
    /// via `create_spawned_session`.
    SubagentSpawned {
        initial_message: Box<IncomingMessage>,
        parent_job_id: aura_model::JobId,
    },
    /// Gracefully shut down this actor.
    Shutdown,
}

/// Mirrors the gateway slash dispatcher's tolerance for casing and
/// trailing arguments so a user typing `/Compact extra` from any
/// channel hits the same control path.
fn is_compact_command(content: &[ContentBlock]) -> bool {
    let Some(ContentBlock::Text(text)) = content.first() else {
        return false;
    };
    let Some(rest) = text.trim().strip_prefix('/') else {
        return false;
    };
    let token = rest.split_whitespace().next().unwrap_or("");
    token.eq_ignore_ascii_case(COMPACT_COMMAND.trim_start_matches('/'))
}

/// One actor per session. Receives messages sequentially through its mailbox.
pub struct AgentActor {
    session: Session,
    agent_loop: AgentLoop,
    response_tx: mpsc::Sender<AgentOutput>,
    job_lifecycle: Arc<JobLifecycle>,
    span_recorder: Arc<SpanRecorder>,
    /// Lifetime token for this actor. Derived as a child of the
    /// process-wide parent token at construction time, so cancelling
    /// the parent (e.g. on `ShutdownSignal::trigger`) cascades down
    /// through every in-flight tool / subagent. `Shutdown` on the
    /// mailbox additionally trips it locally for cooperative shutdown
    /// of just this actor.
    actor_token: CancellationToken,
}

impl AgentActor {
    pub fn new(
        session: Session,
        agent_loop: AgentLoop,
        response_tx: mpsc::Sender<AgentOutput>,
        job_lifecycle: Arc<JobLifecycle>,
        span_recorder: Arc<SpanRecorder>,
        parent_token: &CancellationToken,
    ) -> Self {
        Self {
            session,
            agent_loop,
            response_tx,
            job_lifecycle,
            span_recorder,
            actor_token: parent_token.child_token(),
        }
    }

    /// Run the actor's message processing loop.
    pub async fn run(mut self, mut mailbox: mpsc::Receiver<AgentMessage>) {
        info!(session_id = %self.session.id, "agent actor started");

        while let Some(msg) = mailbox.recv().await {
            match msg {
                AgentMessage::UserInput(incoming) => {
                    if let Err(e) = self.handle_user_input(*incoming).await {
                        error!(
                            session_id = %self.session.id,
                            error = %e,
                            "failed to handle user input"
                        );
                    }
                }
                AgentMessage::CronTrigger { job_id, prompt } => {
                    debug!(session_id = %self.session.id, job_id = %job_id, "received cron trigger");
                    let job_input = JobInput::Cron {
                        action_payload: serde_json::json!({
                            "cron_job_id": job_id,
                            "prompt": prompt,
                        }),
                    };
                    if let Err(e) = self
                        .dispatch_prompt(&prompt, "cron", &job_id, job_input, None)
                        .await
                    {
                        error!(
                            session_id = %self.session.id,
                            job_id = %job_id,
                            error = %e,
                            "failed to handle cron trigger"
                        );
                    }
                }
                AgentMessage::SubagentSpawned {
                    initial_message,
                    parent_job_id,
                } => {
                    if let Err(e) = self
                        .handle_subagent_spawned(*initial_message, parent_job_id)
                        .await
                    {
                        error!(
                            session_id = %self.session.id,
                            error = %e,
                            "failed to handle subagent spawn"
                        );
                    }
                }
                AgentMessage::Shutdown => {
                    debug!(session_id = %self.session.id, "actor shutting down");
                    // Trip the actor's lifetime token so any in-flight
                    // tool / subagent observes the cancel even if the
                    // mailbox-drain happens while a job is running.
                    self.actor_token.cancel();
                    break;
                }
            }
        }
        info!(session_id = %self.session.id, "agent actor stopped");
    }

    /// Run the agent loop. Terminal-state notification is published by
    /// `JobLifecycle` itself on the broadcast bus
    /// (`subscribe_terminal_events`); the actor no longer emits a
    /// piggy-back signal on the response channel. Used by every
    /// handler that delegates job lifecycle to `agent_loop.run`
    /// (UserInput, SubagentSpawned, cron prompt dispatch). Returns
    /// the loop's response on `Ok`; caller is responsible for sending
    /// it to the response channel.
    async fn run_agent_loop(
        &mut self,
        job_input: JobInput,
        content: Vec<ContentBlock>,
        parent_job_id: Option<aura_model::JobId>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<OutgoingMessage> {
        self.agent_loop
            .run(
                &mut self.session,
                job_input,
                content,
                &self.job_lifecycle,
                &self.span_recorder,
                parent_job_id,
                delta_tx,
                self.actor_token.child_token(),
            )
            .await
    }

    /// Dispatch a system-generated prompt (cron or routine) through the
    /// agent loop and send the response to the output channel.
    ///
    /// `job_input` records the trigger provenance (e.g. `JobInput::Cron`
    /// or `JobInput::System`) — this MUST match the session's root
    /// trigger or `JobLifecycle::start_job` will reject it. The
    /// synthesized `[{source}:{source_id}] {prompt}` content is what the
    /// LLM sees; the `JobInput` is purely for the Job record.
    async fn dispatch_prompt(
        &mut self,
        prompt: &str,
        source: &str,
        source_id: &str,
        job_input: JobInput,
        parent_job_id: Option<aura_model::JobId>,
    ) -> anyhow::Result<()> {
        let content = vec![ContentBlock::Text(format!(
            "[{source}:{source_id}] {prompt}"
        ))];
        let response = self
            .run_agent_loop(job_input, content, parent_job_id, None)
            .await?;
        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send {source} response to channel");
        }
        Ok(())
    }

    async fn handle_user_input(&mut self, incoming: IncomingMessage) -> anyhow::Result<()> {
        let content = incoming.message.content;
        if is_compact_command(&content) {
            return self.handle_compact().await;
        }
        // Pass a clone of the response channel so the loop can stream
        // text deltas as `AgentOutput::Delta` while the final assembled
        // message still flows through the normal path.
        let response = self
            .run_agent_loop(
                JobInput::UserChat {
                    content: content.clone(),
                },
                content,
                None,
                Some(self.response_tx.clone()),
            )
            .await?;
        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send response to channel");
        }
        Ok(())
    }

    /// `/compact` is a control command, not an assistant turn, so the
    /// confirmation goes back as `Notice` (out-of-band, off-transcript)
    /// rather than `Message`.
    async fn handle_compact(&mut self) -> anyhow::Result<()> {
        let text = self
            .agent_loop
            .compact_now(
                &mut self.session,
                &self.job_lifecycle,
                &self.span_recorder,
                None,
                self.actor_token.child_token(),
            )
            .await?;
        let notice = AgentOutput::Notice {
            session_id: self.session.id.to_string(),
            user_id: self.session.user.id.clone(),
            channel: self.session.channel.clone(),
            level: NoticeLevel::Info,
            text,
        };
        if let Err(e) = self.response_tx.send(notice).await {
            warn!(error = %e, "failed to send compact notice to channel");
        }
        Ok(())
    }

    /// Run the agent loop for a subagent-spawned session. Distinct from
    /// `handle_user_input` because the JobInput must be `Spawned` (not
    /// `UserChat`) so `JobLifecycle::start_job`'s allowed-for check
    /// passes regardless of the inherited trigger kind.
    async fn handle_subagent_spawned(
        &mut self,
        incoming: IncomingMessage,
        parent_job_id: aura_model::JobId,
    ) -> anyhow::Result<()> {
        let content = incoming.message.content;
        let response = self
            .run_agent_loop(
                JobInput::Spawned {
                    initial_prompt: content.clone(),
                },
                content,
                Some(parent_job_id),
                Some(self.response_tx.clone()),
            )
            .await?;
        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send subagent response");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(s.to_string())]
    }

    #[test]
    fn matches_bare_command() {
        assert!(is_compact_command(&text("/compact")));
        assert!(is_compact_command(&text("  /compact  ")));
    }

    #[test]
    fn is_case_insensitive_and_ignores_trailing_args() {
        assert!(is_compact_command(&text("/CompAct")));
        assert!(is_compact_command(&text("/compact whatever extra")));
    }

    #[test]
    fn rejects_other_inputs() {
        assert!(!is_compact_command(&text("compact")));
        assert!(!is_compact_command(&text("/compaction")));
        assert!(!is_compact_command(&text("see /compact below")));
        assert!(!is_compact_command(&text("")));
        assert!(!is_compact_command(&[]));
        assert!(!is_compact_command(&[ContentBlock::ToolResult {
            tool_use_id: "x".into(),
            content: "/compact".into(),
        }]));
    }
}
