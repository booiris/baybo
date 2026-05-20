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

use crate::{BackgroundCompressionPayload, ContentBlock, JobId, ModelTier, SessionId, SpanId};

/// Tool name the LLM emits to spawn a subagent.
pub const SPAWN_SUBAGENT_TOOL_NAME: &str = "spawn_subagent";

/// `ChannelType` value stamped on every subagent's `User`. Lets channel
/// sidecars filter subagent traffic.
pub const SUBAGENT_CHANNEL_TAG: &str = "subagent";

/// Prefix the router stamps on a background subagent's `handle_id` so
/// trace viewers and the parent LLM can recognise the dispatch mode at
/// a glance. Surfaced as a const because the same id flows into
/// `PendingSubagentResult.handle_id` and the parent's notification
/// preamble — two sites at minimum, three when the CLI starts listing
/// in-flight handles.
pub const BACKGROUND_SUBAGENT_HANDLE_PREFIX: &str = "bg-";

/// Opening line of the system-reminder preamble the parent actor
/// prepends to the next user turn after one or more background
/// subagents land their results. Lifted so the actor that writes it
/// and integration tests that grep for it share one source of truth.
pub const BACKGROUND_NOTIFICATIONS_PREAMBLE: &str =
    "[background subagent notifications — work that finished since your last turn]\n";

/// Closing line of the same preamble.
pub const BACKGROUND_NOTIFICATIONS_POSTAMBLE: &str = "[end of background notifications]\n\n";

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
///
/// The tool resolves the `subagent_type` name into a concrete
/// `SubagentProfile` BEFORE producing this envelope and stamps the
/// profile's `system_prompt` here. The router consumes the prompt
/// verbatim — no further registry lookup happens agent-side.
///
/// Field naming follows Claude Code's Agent tool: `task_summary` is
/// the short 3-5 word title surfaced in traces, while `prompt` is the
/// self-contained brief that becomes the child's first user message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSpawnRequest {
    /// Profile name the parent LLM emitted as `subagent_type`. Surfaced
    /// in trace/audit only — the resolved [`Self::system_prompt`]
    /// already encodes behaviour. Stored as a plain `String` so this
    /// crate stays a leaf (no dependency on `aura-subagent`).
    pub subagent_type: String,
    /// Child actor's full system prompt. REPLACES the parent's Soul —
    /// the profile author owns identity / security / output contracts.
    pub system_prompt: String,
    /// 3-5 word summary the parent LLM authored. Trace display only;
    /// not part of [`Self::initial_prompt`].
    pub task_summary: String,
    /// Self-contained brief — becomes the child actor's first user
    /// message (along with any `must_include_context` bullets).
    pub prompt: String,
    #[serde(default)]
    pub must_include_context: Vec<String>,
    pub timeout: Duration,
    /// Coarse model tier. Resolution precedence (highest first):
    /// explicit [`Self::llm`] → this field → profile's `default_tier`
    /// → pool default. Wired up in task 6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_tier: Option<ModelTier>,
    /// Explicit `llm[*].name` override (super-escape). Beats
    /// `model_tier`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<String>,
    /// Fire-and-forget mode. Reserved for task 8 — when `true` the
    /// router will surface a handle id and parent the child's wait
    /// task on the parent actor's token rather than the parent job's
    /// token. The current router rejects this until that path lands.
    #[serde(default)]
    pub background: bool,
    /// Root session id used by the dispatcher's fan-out limiter.
    /// Stamped by `spawn_subagent` after it walks the lineage chain
    /// (and reserves a slot in the limiter), so the router's wait
    /// task can release the slot on terminal without re-walking.
    /// `None` only on synthesized requests from tests / future
    /// programmatic call sites that don't go through the limiter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_out_root: Option<SessionId>,
}

impl SubagentSpawnRequest {
    /// Render the child's first user message — `prompt` with any
    /// `must_include_context` notes appended as bullets.
    pub fn initial_prompt(&self) -> String {
        if self.must_include_context.is_empty() {
            return self.prompt.clone();
        }
        let mut text = self.prompt.clone();
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentExitStatus {
    Completed,
    /// Parent's `CancellationToken` was tripped before the child
    /// returned. Includes the case where a higher ancestor cancelled.
    Cancelled,
    Failed { reason: String },
    Timeout,
}

/// Persistent record of a `background: true` subagent that completed
/// while the parent actor was between turns. Held on
/// [`crate::SessionState::pending_subagent_results`] until the next
/// user input drains it.
///
/// `handle_id` is the synthetic identifier the spawning tool minted
/// and surfaced to the parent LLM as the "dispatched" handle, so
/// later turn-prepend messages can name the same id the parent saw
/// at dispatch time. `images` is empty for non-completed exits so
/// failure noise can't leak attachment context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSubagentResult {
    pub handle_id: String,
    pub subagent_type: String,
    pub task_summary: String,
    pub child_session_id: SessionId,
    pub final_text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ContentBlock>,
    pub status: SubagentExitStatus,
}

/// Split of a [`SubagentResult`] into the components the parent's
/// tool boundary needs:
///  * `text` — the always-non-empty string that will populate the
///    `tool_result` content the parent LLM sees on its next turn.
///  * `llm_images` — `ContentBlock::Image` entries from the
///    completed-subagent's final message that vision-capable parent
///    LLMs should see. Empty for non-completed terminations.
///
/// Lives in `aura-model` because the values are pure data and both
/// `aura-tools` (the tool boundary that builds `ToolOutput`) and the
/// router/wait routine reach for them. Non-image, non-text blocks
/// (thinking, tool_use, tool_result) are intentionally dropped —
/// they're internal subagent state and don't belong on the
/// parent-visible boundary.
#[derive(Debug, Clone, Default)]
pub struct SubagentReturn {
    pub text: String,
    pub llm_images: Vec<ContentBlock>,
}

impl SubagentResult {
    /// Convenience constructor for the early-return failure cases
    /// (channel closed, parent session missing, …) where no child
    /// session was ever created.
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            child_session_id: SessionId::from(""),
            final_content: None,
            status: SubagentExitStatus::Failed {
                reason: reason.into(),
            },
        }
    }

    /// Split this result into text + image components for the parent
    /// tool boundary. See [`SubagentReturn`] for the contract.
    pub fn split_for_parent(&self) -> SubagentReturn {
        match (&self.status, &self.final_content) {
            (SubagentExitStatus::Completed, Some(blocks)) => {
                let mut text = extract_text(blocks);
                let llm_images: Vec<ContentBlock> = blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Image { .. }))
                    .cloned()
                    .collect();
                if text.is_empty() {
                    text = if llm_images.is_empty() {
                        "[subagent completed without producing a final message]".to_string()
                    } else {
                        "[subagent returned image attachments only]".to_string()
                    };
                }
                SubagentReturn { text, llm_images }
            }
            (SubagentExitStatus::Completed, None) => SubagentReturn {
                text: "[subagent completed without producing a final message]".to_string(),
                llm_images: Vec::new(),
            },
            (SubagentExitStatus::Cancelled, _) => SubagentReturn {
                text: "[subagent cancelled by parent before producing a result]".to_string(),
                llm_images: Vec::new(),
            },
            (SubagentExitStatus::Failed { reason }, _) => SubagentReturn {
                text: format!("[subagent failed: {reason}]"),
                llm_images: Vec::new(),
            },
            (SubagentExitStatus::Timeout, _) => SubagentReturn {
                text: "[subagent exceeded its declared timeout]".to_string(),
                llm_images: Vec::new(),
            },
        }
    }

    /// Backwards-compatible flat text rendering. New call sites should
    /// reach for [`Self::split_for_parent`] so image attachments survive
    /// the boundary; this stays for log/audit dumps where only text
    /// matters.
    pub fn to_tool_result_text(&self) -> String {
        self.split_for_parent().text
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

#[cfg(test)]
mod tests {
    use super::*;

    fn req(prompt: &str, ctx: Vec<&str>) -> SubagentSpawnRequest {
        SubagentSpawnRequest {
            subagent_type: "general-purpose".into(),
            system_prompt: "You are a test subagent.".into(),
            task_summary: "test".into(),
            prompt: prompt.into(),
            must_include_context: ctx.into_iter().map(String::from).collect(),
            timeout: Duration::from_secs(60),
            model_tier: None,
            llm: None,
            background: false,
            fan_out_root: None,
        }
    }

    #[test]
    fn initial_prompt_without_context_is_prompt_only() {
        let r = req("just the task", vec![]);
        assert_eq!(r.initial_prompt(), "just the task");
    }

    #[test]
    fn initial_prompt_with_context_appends_bullets() {
        let r = req("do X", vec!["fact a", "fact b"]);
        let p = r.initial_prompt();
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
    fn split_for_parent_passes_through_image_attachments() {
        let img = ContentBlock::Image {
            blob: crate::BlobRef {
                blob_id: "b-1".into(),
            },
            mime_type: "image/png".into(),
        };
        let r = SubagentResult {
            child_session_id: SessionId::from("child-1"),
            final_content: Some(vec![
                ContentBlock::Text("found this".into()),
                img.clone(),
            ]),
            status: SubagentExitStatus::Completed,
        };
        let parts = r.split_for_parent();
        assert_eq!(parts.text, "found this");
        assert_eq!(parts.llm_images, vec![img]);
    }

    #[test]
    fn split_for_parent_image_only_returns_canned_text() {
        let img = ContentBlock::Image {
            blob: crate::BlobRef {
                blob_id: "b-1".into(),
            },
            mime_type: "image/png".into(),
        };
        let r = SubagentResult {
            child_session_id: SessionId::from("child-1"),
            final_content: Some(vec![img.clone()]),
            status: SubagentExitStatus::Completed,
        };
        let parts = r.split_for_parent();
        assert!(parts.text.contains("image attachments only"));
        assert_eq!(parts.llm_images, vec![img]);
    }

    #[test]
    fn split_for_parent_failure_drops_images() {
        let img = ContentBlock::Image {
            blob: crate::BlobRef {
                blob_id: "b-1".into(),
            },
            mime_type: "image/png".into(),
        };
        let r = SubagentResult {
            child_session_id: SessionId::from("child-1"),
            final_content: Some(vec![img]),
            status: SubagentExitStatus::Failed { reason: "boom".into() },
        };
        let parts = r.split_for_parent();
        assert!(parts.text.contains("boom"));
        assert!(parts.llm_images.is_empty());
    }

    #[test]
    fn split_for_parent_drops_non_text_non_image_blocks() {
        let r = SubagentResult {
            child_session_id: SessionId::from("child-1"),
            final_content: Some(vec![
                ContentBlock::Text("a".into()),
                ContentBlock::Thinking {
                    id: None,
                    content: vec![],
                },
                ContentBlock::ToolUse {
                    id: "tu".into(),
                    name: "t".into(),
                    input: serde_json::json!({}),
                    signature: None,
                },
            ]),
            status: SubagentExitStatus::Completed,
        };
        let parts = r.split_for_parent();
        assert_eq!(parts.text, "a");
        assert!(parts.llm_images.is_empty());
    }

    #[test]
    fn tool_result_text_failed_includes_reason() {
        let r = SubagentResult::failed("boom");
        let s = r.to_tool_result_text();
        assert!(s.contains("boom"));
        assert!(matches!(&r.status, SubagentExitStatus::Failed { reason } if reason == "boom"));
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
