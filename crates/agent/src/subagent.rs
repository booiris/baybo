//! Subagent spawn + result-collection runtime.
//!
//! When a parent agent's LLM emits `tool_use { name: "spawn_subagent",
//! ... }`, [`AgentLoop`] short-circuits the regular tool-executor path
//! and routes through a [`SubagentRuntime`] instead. The runtime is
//! responsible for:
//!
//!  1. Constructing a child [`Session`] with the right `Lineage`
//!     (subagent + parent_session_id + parent_job_id).
//!  2. Spawning a child actor via the caller-supplied
//!     [`SubagentActorSpawner`] closure (mirrors the top-level
//!     [`crate::router::ActorSpawner`] but isolated so the subagent
//!     path can override per-spawn behavior — e.g. a different
//!     output channel size, custom hooks).
//!  3. Driving the initial prompt into the child mailbox and waiting
//!     for the child to return a final [`AgentOutput::Message`] (or
//!     hit the spawn timeout / be cancelled by the parent's token
//!     tree).
//!  4. Returning a [`SubagentResult`] the parent injects as the
//!     `tool_result` for its next LLM iteration.
//!
//! The implementation is **synchronous from the parent's
//! perspective**: parent's LLM iteration step blocks on the
//! [`SubagentRuntime::spawn`] future until the child terminates.
//! Cancellation is via [`tokio_util::sync::CancellationToken`] — the
//! request carries the parent's token; the runtime constructs a
//! child token from it so propagation is automatic on cancel.

use std::sync::Arc;
use std::time::Duration;

use aura_channels::{AgentOutput, IncomingMessage, JobOutcome, Message};
use aura_model::{
    ChannelType, ContentBlock, JobId, Lineage, LineageKind, MessageMetadata, SessionId, User,
};
use aura_session::SessionManager;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::actor::AgentMessage;

/// Caller-supplied closure that builds a child actor and returns its
/// mailbox sender. Mirrors [`crate::router::ActorSpawner`] but kept
/// separate so the subagent path can swap in spawn-specific knobs
/// (e.g. tighter output buffer, preconfigured hooks).
pub type SubagentActorSpawner = Arc<
    dyn Fn(aura_model::Session, mpsc::Sender<AgentOutput>) -> mpsc::Sender<AgentMessage>
        + Send
        + Sync,
>;

/// What the parent LLM asks for when it emits `spawn_subagent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSpawnRequest {
    /// Free-text task statement the LLM wrote.
    pub task_description: String,
    /// Optional context references — span IDs the parent wants the
    /// child to keep visible, and / or free-text bullets. Today these
    /// are appended to the child's first user message verbatim;
    /// rendering trace-span content into the child prompt is a
    /// follow-up (`task_description` + spans = compression step).
    #[serde(default)]
    pub must_include_context: Vec<String>,
    /// Hard wait limit. Exceeding this returns
    /// `SubagentExitStatus::Timeout` and trips the parent's token
    /// (the child's descendant tokens cascade automatically).
    pub timeout: Duration,
}

/// What the parent receives back from the runtime.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub child_session_id: SessionId,
    pub final_content: Option<Vec<ContentBlock>>,
    pub status: SubagentExitStatus,
}

#[derive(Debug, Clone)]
pub enum SubagentExitStatus {
    /// Child returned a final message and cleanly shut down.
    Completed,
    /// Parent's `CancellationToken` was tripped before the child
    /// returned. Includes the case where a higher ancestor cancelled.
    Cancelled,
    /// Child's runtime erred (mailbox dropped, actor panicked, etc.).
    Failed(String),
    /// `spawn`'s `timeout` elapsed before the child returned.
    Timeout,
}

impl SubagentResult {
    /// Render this result into a synthetic `tool_result` payload the
    /// parent's next LLM iteration sees. On non-completed exits, the
    /// content names the failure mode so the LLM can surface it to
    /// the user.
    pub fn to_tool_result_text(&self) -> String {
        match (&self.status, &self.final_content) {
            (SubagentExitStatus::Completed, Some(blocks)) => {
                aura_llm::multimodal::extract_text(blocks)
            }
            (SubagentExitStatus::Completed, None) => {
                "[subagent completed without producing a final message]".to_string()
            }
            (SubagentExitStatus::Cancelled, _) => {
                "[subagent cancelled by parent before producing a result]".to_string()
            }
            (SubagentExitStatus::Failed(reason), _) => {
                format!("[subagent failed: {reason}]")
            }
            (SubagentExitStatus::Timeout, _) => {
                "[subagent exceeded its declared timeout]".to_string()
            }
        }
    }
}

/// Pre-created child session. Returned by [`SubagentRuntime::prepare`]
/// so the parent can open its `StepKind::Subagent` step with the real
/// child session id instead of a placeholder.
pub struct PreparedSubagent {
    pub child_session: aura_model::Session,
}

/// Spawn a child agent session and wait for it to terminate.
///
/// Two-phase API:
/// 1. [`prepare`](Self::prepare) synchronously creates the child session
///    row. Callers use the returned id to open the
///    `StepKind::Subagent` step before dispatch.
/// 2. [`run`](Self::run) drives the child to terminal state. Cancellation
///    is via [`tokio_util::sync::CancellationToken`] — the runtime
///    derives a child token so the parent's cancel cascades the entire
///    subagent subtree.
#[async_trait::async_trait]
pub trait SubagentRuntime: Send + Sync {
    async fn prepare(
        &self,
        parent_session: &aura_model::Session,
        parent_job_id: JobId,
        request: &SubagentSpawnRequest,
    ) -> Result<PreparedSubagent, String>;

    async fn run(
        &self,
        prepared: PreparedSubagent,
        request: SubagentSpawnRequest,
        parent_token: CancellationToken,
    ) -> SubagentResult;
}

/// In-process implementation of `SubagentRuntime`.
///
/// Holds the `SessionManager` (to create the child session row) and
/// a `SubagentActorSpawner` closure that knows how to construct a
/// fully-wired child actor from a session + output channel.
pub struct LocalSubagentRuntime {
    sessions: Arc<SessionManager>,
    spawn_actor: SubagentActorSpawner,
}

impl LocalSubagentRuntime {
    pub fn new(sessions: Arc<SessionManager>, spawn_actor: SubagentActorSpawner) -> Self {
        Self {
            sessions,
            spawn_actor,
        }
    }

    fn build_child_user(parent_user: &User) -> User {
        // Inherit the parent's user identity but flag the channel as
        // a subagent surface so any sidecar that conditionally renders
        // by channel can ignore subagent traffic.
        User {
            id: parent_user.id.clone(),
            name: parent_user.name.clone(),
            channel: ChannelType::from("subagent"),
        }
    }

    fn build_initial_prompt(req: &SubagentSpawnRequest) -> Vec<ContentBlock> {
        let mut text = req.task_description.clone();
        if !req.must_include_context.is_empty() {
            text.push_str("\n\nMust-include context:\n");
            for note in &req.must_include_context {
                text.push_str("- ");
                text.push_str(note);
                text.push('\n');
            }
        }
        vec![ContentBlock::Text(text)]
    }
}

#[async_trait::async_trait]
impl SubagentRuntime for LocalSubagentRuntime {
    async fn prepare(
        &self,
        parent_session: &aura_model::Session,
        parent_job_id: JobId,
        _request: &SubagentSpawnRequest,
    ) -> Result<PreparedSubagent, String> {
        let child_user = Self::build_child_user(&parent_session.user);
        let child_channel = child_user.channel.clone();
        let lineage = Lineage {
            parent_session_id: parent_session.id.clone(),
            parent_job_id,
            kind: LineageKind::Subagent,
        };
        let child_session = self
            .sessions
            .create_spawned_session(child_user, child_channel, parent_session, lineage)
            .await
            .map_err(|e| format!("create child session: {e}"))?;
        Ok(PreparedSubagent { child_session })
    }

    async fn run(
        &self,
        prepared: PreparedSubagent,
        request: SubagentSpawnRequest,
        parent_token: CancellationToken,
    ) -> SubagentResult {
        let child_session = prepared.child_session;
        let parent_job_id = child_session
            .lineage
            .as_ref()
            .map(|l| l.parent_job_id)
            .unwrap_or_default();
        let child_user = child_session.user.clone();
        let child_channel = child_session.channel.clone();

        // Spawn the child actor with our own response channel so we
        // can capture the final message synchronously.
        let (output_tx, mut output_rx) = mpsc::channel::<AgentOutput>(64);
        let mailbox = (self.spawn_actor)(child_session.clone(), output_tx);

        // Build the initial prompt and dispatch via SubagentSpawned
        // (not UserInput) so the child actor's handler runs
        // `agent_loop.run` with `JobInput::Spawned` — passes the
        // allowed-for check regardless of inherited trigger kind.
        let initial_content = Self::build_initial_prompt(&request);
        let incoming = IncomingMessage {
            message: Message {
                id: format!(
                    "subagent-init-{}",
                    Utc::now().timestamp_nanos_opt().unwrap_or(0)
                ),
                session_id: child_session.id.to_string(),
                channel: child_channel.clone(),
                sender: child_user.clone(),
                content: initial_content,
                timestamp: Utc::now(),
                reply_to: None,
                metadata: MessageMetadata::default(),
            },
        };
        if let Err(e) = mailbox
            .send(AgentMessage::SubagentSpawned {
                initial_message: Box::new(incoming),
                parent_job_id,
            })
            .await
        {
            return SubagentResult {
                child_session_id: child_session.id,
                final_content: None,
                status: SubagentExitStatus::Failed(format!("dispatch child input: {e}")),
            };
        }

        // 4. Wait for the child to terminate — or for timeout / parent
        //    cancellation. The child actor emits both
        //    `AgentOutput::Message` (user-visible reply) and
        //    `AgentOutput::JobCompleted` (internal terminal-state
        //    signal); we capture the most recent message while
        //    streaming, and unblock on `JobCompleted`. Driving
        //    completion off the explicit terminal signal is what makes
        //    the multi-job child path correct: a failed child whose
        //    `agent_loop.run` errored before producing a `Message`
        //    still surfaces as `JobCompleted { Failed }` rather than
        //    hanging until the spawn timeout.
        let child_token = parent_token.child_token();
        let mut captured: Option<Vec<ContentBlock>> = None;
        let wait_result = tokio::time::timeout(request.timeout, async {
            loop {
                tokio::select! {
                    _ = child_token.cancelled() => {
                        return Err(SubagentExitStatus::Cancelled);
                    }
                    msg = output_rx.recv() => {
                        match msg {
                            Some(AgentOutput::Message(m)) => {
                                captured = Some(m.content);
                            }
                            Some(AgentOutput::JobCompleted { outcome, .. }) => {
                                return match outcome {
                                    JobOutcome::Completed => Ok(captured.take()),
                                    JobOutcome::Failed => Err(SubagentExitStatus::Failed(
                                        "child job failed".into(),
                                    )),
                                    JobOutcome::Cancelled => Err(SubagentExitStatus::Cancelled),
                                };
                            }
                            // Skip progress / delta / notice — wait for
                            // the terminal `JobCompleted` signal.
                            Some(_) => continue,
                            None => return Err(SubagentExitStatus::Failed(
                                "child output channel closed without terminal signal".into(),
                            )),
                        }
                    }
                }
            }
        })
        .await;

        // 5. Drain by sending Shutdown so the child actor releases its
        //    mailbox + flushes anything in flight. Ignore send error —
        //    if the actor already exited that is fine.
        let _ = mailbox.send(AgentMessage::Shutdown).await;

        match wait_result {
            Ok(Ok(content)) => SubagentResult {
                child_session_id: child_session.id,
                final_content: content,
                status: SubagentExitStatus::Completed,
            },
            Ok(Err(status)) => SubagentResult {
                child_session_id: child_session.id,
                final_content: None,
                status,
            },
            Err(_elapsed) => {
                // On timeout, trip the child token so any nested
                // subagent the child spawned cancels too. The
                // descendant tree cascades automatically because each
                // nested SubagentRuntime call uses
                // `parent_token.child_token()`.
                child_token.cancel();
                SubagentResult {
                    child_session_id: child_session.id,
                    final_content: None,
                    status: SubagentExitStatus::Timeout,
                }
            }
        }
    }
}

/// Reserved tool name — when AgentLoop sees this in a `tool_use` it
/// short-circuits the normal tool path and routes through
/// [`SubagentRuntime`] instead.
pub const SPAWN_SUBAGENT_TOOL_NAME: &str = "spawn_subagent";

/// Parse the JSON arguments emitted alongside `spawn_subagent` into a
/// typed [`SubagentSpawnRequest`]. Missing `timeout_secs` defaults to
/// 600s; values <1s are clamped to 1s.
pub fn parse_spawn_request(value: &serde_json::Value) -> Result<SubagentSpawnRequest, String> {
    let task = value
        .get("task_description")
        .and_then(|v| v.as_str())
        .ok_or("missing task_description")?
        .to_string();
    let context = value
        .get("must_include_context")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let timeout_secs = value
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(600);
    let timeout = Duration::from_secs(timeout_secs.max(1));
    Ok(SubagentSpawnRequest {
        task_description: task,
        must_include_context: context,
        timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_spawn_request_minimal() {
        let v = json!({"task_description": "do the thing"});
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(req.task_description, "do the thing");
        assert_eq!(req.must_include_context.len(), 0);
        assert_eq!(req.timeout, Duration::from_secs(600));
    }

    #[test]
    fn parse_spawn_request_full() {
        let v = json!({
            "task_description": "investigate",
            "must_include_context": ["span:abc", "user wanted X"],
            "timeout_secs": 30,
        });
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(req.task_description, "investigate");
        assert_eq!(req.must_include_context.len(), 2);
        assert_eq!(req.timeout, Duration::from_secs(30));
    }

    #[test]
    fn parse_spawn_request_rejects_missing_task() {
        let v = json!({"timeout_secs": 60});
        assert!(parse_spawn_request(&v).is_err());
    }

    #[test]
    fn timeout_clamped_to_at_least_1s() {
        let v = json!({"task_description": "x", "timeout_secs": 0});
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(req.timeout, Duration::from_secs(1));
    }

    #[test]
    fn to_tool_result_text_completed_extracts_text() {
        let r = SubagentResult {
            child_session_id: SessionId::from("c"),
            final_content: Some(vec![ContentBlock::Text("hello".into())]),
            status: SubagentExitStatus::Completed,
        };
        assert_eq!(r.to_tool_result_text(), "hello");
    }

    #[test]
    fn to_tool_result_text_failure_modes_carry_reason() {
        let r = SubagentResult {
            child_session_id: SessionId::from("c"),
            final_content: None,
            status: SubagentExitStatus::Failed("boom".into()),
        };
        assert!(r.to_tool_result_text().contains("boom"));

        let r = SubagentResult {
            child_session_id: SessionId::from("c"),
            final_content: None,
            status: SubagentExitStatus::Timeout,
        };
        assert!(r.to_tool_result_text().contains("timeout"));
    }
}
