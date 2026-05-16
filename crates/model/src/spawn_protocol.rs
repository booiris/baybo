//! Cross-crate spawn protocol types.
//!
//! `aura-tools` (the `spawn_subagent` builtin) and `aura-agent` (the
//! router, agent loop, and child wait routine) both need to construct
//! and pattern-match these values. They live here in `aura-model` so
//! the dependency direction stays one-way (`aura-tools` →
//! `aura-model` ← `aura-agent`) without an intermediate trait + sink.
//!
//! Two value families share this module:
//!  * [`SystemSpawnRequest`] — the envelope the router consumes on its
//!    `system_trigger_rx` arm. Today's variants cover background
//!    summary compression and child-subagent dispatch.
//!  * `Subagent*` — the per-spawn request / parent-context / result /
//!    exit-status quadruple the LLM-facing `spawn_subagent` tool
//!    exchanges with the router.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{BackgroundCompressionPayload, ContentBlock, JobId, SessionId, SpanId};

/// Tool name the LLM emits to spawn a subagent.
pub const SPAWN_SUBAGENT_TOOL_NAME: &str = "spawn_subagent";

/// `ChannelType` value stamped on every subagent's `User`. Lets channel
/// sidecars filter subagent traffic.
pub const SUBAGENT_CHANNEL_TAG: &str = "subagent";

const DEFAULT_SUBAGENT_TIMEOUT_SECS: u64 = 600;

/// Hard upper bound on a single `spawn_subagent` wait, in seconds.
/// Shared with the `spawn_subagent` tool's `Tool::max_timeout` so the
/// router's waiter cannot outlive the executor's wall-clock cap.
pub const MAX_SUBAGENT_TIMEOUT_SECS: u64 = 3600;

/// Request emitted by `AgentLoop`'s parent-side trigger gate and by
/// the `spawn_subagent` tool, consumed by `Router`'s `system_trigger_rx`
/// arm. Senders push onto an `mpsc::Sender<SystemSpawnRequest>`; the
/// router does the session-create + actor-spawn + mailbox-dispatch.
///
/// `parent_actor_token` is the lifetime token of whatever component
/// owns this spawn (parent actor for background-compression, parent
/// per-job token for subagent dispatch). The router derives the
/// spawned actor's `actor_token` as a child of it, so cancelling the
/// parent cascades into the child via the `tokio_util` token tree
/// — no explicit `Shutdown` mailbox dance required.
#[derive(Debug)]
pub enum SystemSpawnRequest {
    BackgroundCompression {
        parent_session_id: SessionId,
        parent_job_id: JobId,
        parent_actor_token: CancellationToken,
        payload: BackgroundCompressionPayload,
    },
    Subagent {
        parent_session_id: SessionId,
        parent_job_id: JobId,
        /// `ToolCall(spawn_subagent)` span the parent emitted this
        /// request from — recorded on the child's `Lineage` so trace
        /// viewers can hop from the parent's tool span to the child's
        /// session, and so multiple sibling subagents from one parent
        /// job stay distinguishable.
        parent_span_id: SpanId,
        parent_actor_token: CancellationToken,
        request: SubagentSpawnRequest,
        result_tx: oneshot::Sender<SubagentResult>,
    },
}

/// What the parent LLM asks for when it calls `spawn_subagent`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSpawnRequest {
    pub task_description: String,
    #[serde(default)]
    pub must_include_context: Vec<String>,
    pub timeout: Duration,
    /// Optional LLM entry-name override for the spawned child.
    /// `None` ⇒ fall back to the pool default at spawn time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<String>,
}

impl SubagentSpawnRequest {
    /// Render the child's first user message — task description with
    /// any `must_include_context` notes appended as bullets.
    pub fn initial_prompt(&self) -> String {
        if self.must_include_context.is_empty() {
            return self.task_description.clone();
        }
        let mut text = self.task_description.clone();
        text.push_str("\n\nMust-include context:\n");
        for note in &self.must_include_context {
            text.push_str("- ");
            text.push_str(note);
            text.push('\n');
        }
        text
    }
}

/// Parent-side context the tool builds from its `ToolContext` and
/// places on the `SystemSpawnRequest::Subagent` envelope.
#[derive(Debug, Clone)]
pub struct SubagentParentContext {
    pub session_id: SessionId,
    pub job_id: JobId,
    /// Parent's `ToolCall(spawn_subagent)` span id — recorded on the
    /// child's `Lineage` so trace viewers can hop from the parent's
    /// tool span to the child session.
    pub span_id: SpanId,
    pub cancel_token: CancellationToken,
}

#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub child_session_id: SessionId,
    pub final_content: Option<Vec<ContentBlock>>,
    pub status: SubagentExitStatus,
}

#[derive(Debug, Clone)]
pub enum SubagentExitStatus {
    Completed,
    /// Parent's `CancellationToken` was tripped before the child
    /// returned. Includes the case where a higher ancestor cancelled.
    Cancelled,
    Failed(String),
    Timeout,
}

impl SubagentResult {
    /// Convenience constructor for the early-return failure cases
    /// (channel closed, parent session missing, …) where no child
    /// session was ever created.
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            child_session_id: SessionId::from(""),
            final_content: None,
            status: SubagentExitStatus::Failed(reason.into()),
        }
    }

    /// Render this result into a synthetic `tool_result` payload the
    /// parent's next LLM iteration sees.
    pub fn to_tool_result_text(&self) -> String {
        match (&self.status, &self.final_content) {
            (SubagentExitStatus::Completed, Some(blocks)) => extract_text(blocks),
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

fn extract_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Raw JSON shape the LLM emits as `spawn_subagent`'s arguments.
/// `timeout_secs` defaults to [`DEFAULT_SUBAGENT_TIMEOUT_SECS`] and is
/// clamped to `[1, MAX_SUBAGENT_TIMEOUT_SECS]` in [`parse_spawn_request`].
#[derive(Debug, Clone, Deserialize)]
struct SpawnParams {
    task_description: String,
    #[serde(default)]
    must_include_context: Vec<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    llm: Option<String>,
}

pub fn parse_spawn_request(value: &serde_json::Value) -> Result<SubagentSpawnRequest, String> {
    let p: SpawnParams = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let secs = p
        .timeout_secs
        .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_SECS)
        .clamp(1, MAX_SUBAGENT_TIMEOUT_SECS);
    Ok(SubagentSpawnRequest {
        task_description: p.task_description,
        must_include_context: p.must_include_context,
        timeout: Duration::from_secs(secs),
        llm: p.llm,
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
        assert!(req.llm.is_none());
    }

    #[test]
    fn parse_spawn_request_full() {
        let v = json!({
            "task_description": "investigate",
            "must_include_context": ["span:abc", "user wanted X"],
            "timeout_secs": 30,
            "llm": "fast",
        });
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(req.task_description, "investigate");
        assert_eq!(req.must_include_context.len(), 2);
        assert_eq!(req.timeout, Duration::from_secs(30));
        assert_eq!(req.llm.as_deref(), Some("fast"));
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
    fn timeout_clamped_to_outer_cap() {
        let v = json!({
            "task_description": "x",
            "timeout_secs": MAX_SUBAGENT_TIMEOUT_SECS + 1
        });
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(req.timeout, Duration::from_secs(MAX_SUBAGENT_TIMEOUT_SECS));
    }

    #[test]
    fn initial_prompt_without_context_is_task_only() {
        let v = json!({"task_description": "just the task"});
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(req.initial_prompt(), "just the task");
    }

    #[test]
    fn initial_prompt_with_context_appends_bullets() {
        let req = SubagentSpawnRequest {
            task_description: "do X".to_string(),
            must_include_context: vec!["fact a".into(), "fact b".into()],
            timeout: Duration::from_secs(60),
            llm: None,
        };
        let p = req.initial_prompt();
        assert!(p.contains("do X"));
        assert!(p.contains("- fact a"));
        assert!(p.contains("- fact b"));
    }

    #[test]
    fn tool_result_text_completed_concatenates_text_blocks() {
        let r = SubagentResult {
            child_session_id: SessionId::from("child-1"),
            final_content: Some(vec![
                ContentBlock::Text("line one".into()),
                ContentBlock::Text("line two".into()),
            ]),
            status: SubagentExitStatus::Completed,
        };
        assert_eq!(r.to_tool_result_text(), "line one\nline two");
    }

    #[test]
    fn tool_result_text_failed_includes_reason() {
        let r = SubagentResult::failed("boom");
        let s = r.to_tool_result_text();
        assert!(s.contains("boom"));
        assert!(matches!(&r.status, SubagentExitStatus::Failed(s) if s == "boom"));
    }

    #[test]
    fn tool_result_text_timeout_is_canned() {
        let r = SubagentResult {
            child_session_id: SessionId::from("child-x"),
            final_content: None,
            status: SubagentExitStatus::Timeout,
        };
        assert!(r.to_tool_result_text().contains("timeout"));
    }
}
