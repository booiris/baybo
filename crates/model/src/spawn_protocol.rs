//! Cross-crate spawn protocol types.
//!
//! `baybo-subagent` (the `spawn_subagent` tool) and `baybo-agent` (the
//! actor-backed spawner, agent loop, and child wait routine) both need to
//! construct and pattern-match these values. They live here in
//! `baybo-model` so the dependency direction stays one-way without an
//! intermediate trait.
//!
//! The `Subagent*` family — the per-spawn request / parent-context /
//! result / exit-status quadruple — is what the LLM-facing
//! `spawn_subagent` tool hands to the `baybo_subagent::SubagentSpawner`
//! capability (its actor-backed impl lives in `baybo-agent`).

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    ContentBlock, InheritedToolContext, ModelTier, SessionId, SpanId, SubagentBackend, TurnId,
};

/// Tool name the LLM emits to spawn a subagent.
pub const SPAWN_SUBAGENT_TOOL_NAME: &str = "spawn_subagent";

/// `ChannelType` value stamped on every subagent's `User`. Lets channel
/// sidecars filter subagent traffic.
pub const SUBAGENT_CHANNEL_TAG: &str = "subagent";

/// Prefix the router stamps on a background subagent's `handle_id` so
/// trace viewers and the parent LLM can recognise the dispatch mode at
/// a glance. Surfaced as a const because the same id flows into
/// `PendingBackgroundResult.handle_id` and the parent's notification
/// preamble — two sites at minimum, three when the CLI starts listing
/// in-flight handles.
pub const BACKGROUND_SUBAGENT_HANDLE_PREFIX: &str = "bg-";

/// Mint a fresh background-job handle (`bg-…`) — the single id shared by a
/// detached command's (or background subagent's) output, its dispatch ack, and
/// its completion notification. One source of truth for the handle shape so the
/// dispatch, conversion, and bash-detach sites can't drift.
pub fn new_background_handle() -> String {
    format!(
        "{BACKGROUND_SUBAGENT_HANDLE_PREFIX}{}",
        uuid::Uuid::new_v4()
    )
}

/// Leading marker of the tool result a *successful* background subagent
/// dispatch returns (the ack with its `bg-…` handle). The agent loop matches
/// it to count grouped members: a router-side dispatch failure comes back as
/// a `[subagent failed: …]` result instead, which must NOT inflate the group's
/// expected count (no escorted result will ever arrive for it). Single source
/// of truth — the producer (`ack_background_dispatch`) and the consumer (the
/// agent loop's group counter) both reference it.
pub const BACKGROUND_DISPATCH_ACK_PREFIX: &str = "[background subagent dispatched]";

/// Shared tail appended to every background-dispatch ack (explicit
/// `background: true` and foreground-timeout conversion alike). Tells the
/// parent LLM the one thing it needs to hear when its work is now detached:
/// yield the turn instead of busy-waiting. Single-sourced so both ack sites
/// carry identical guidance — the observed failure was a model that, having
/// backgrounded its only task, looped on `sleep` + `JobList` for minutes
/// because the ack merely described the handoff instead of directing it.
pub const BACKGROUND_DISPATCH_YIELD_GUIDANCE: &str = "It runs in the background now. END YOUR TURN unless you have other independent work to do right now — do NOT sleep and do NOT poll JobList to wait for it. The runtime re-invokes you with its result prepended to a fresh turn once it finishes; that result can ONLY arrive after this turn ends, so waiting here cannot surface it and only delays it.";

/// What the parent LLM asks for when it calls `spawn_subagent`.
///
/// The tool validates `subagent_type` against the registry BEFORE
/// producing this envelope (rejecting unknown types and canonicalizing
/// the name), but the resolved system prompt does NOT ride along: the
/// child's `ContextManager` resolves the name back to a prompt via the
/// subagent profile registry at seed time, and re-resolves it on
/// compaction (see the `subagent_type` field).
///
/// Field naming follows Claude Code's Agent tool: `task_summary` is
/// the short 3-5 word title surfaced in traces, while `prompt` is the
/// self-contained brief that becomes the child's first user message.
/// What to do when a *foreground* subagent exceeds its foreground wait.
/// Only consulted for foreground spawns from a user-facing session;
/// `background: true` spawns and non-user parents ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnTimeout {
    /// Detach and keep running; deliver the result via a notification
    /// turn when it finishes. The default.
    #[default]
    Background,
    /// Cancel the child and surface a timeout to the parent.
    Kill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentSpawnRequest {
    /// Profile name the parent LLM emitted. The child's `ContextManager`
    /// resolves it back to a system prompt via the subagent profile registry
    /// (re-resolved on compaction), so the profile owns the child's identity
    /// for the session's life. Stored as a plain `String` so this crate stays
    /// a leaf (no dependency on `baybo-subagent`).
    pub subagent_type: String,
    /// The tools the profile named, already resolved — the spawn tool holds
    /// the profile, this crate cannot (it is a leaf). Pinned onto the child's
    /// session beside `subagent_type` so the list is fixed at genesis rather
    /// than re-derived on every hydration. `None` ⇒ the profile named none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_allowlist: Option<Vec<String>>,
    /// 3-5 word summary the parent LLM authored. Trace display only;
    /// not part of the child's initial prompt.
    pub task_summary: String,
    /// Self-contained brief — becomes the child actor's first user
    /// message.
    pub prompt: String,
    /// Coarse model tier for the Baybo backend. Resolution precedence
    /// (highest first): this field → profile's `default_tier` → pool
    /// default. Ignored for the External backend, which runs its own
    /// model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_tier: Option<ModelTier>,
    /// Fire-and-forget mode. When `true` the router surfaces a handle
    /// id and parents the child's wait task on the parent actor's
    /// token rather than the parent turn's token, so the child outlives
    /// the dispatching turn and escorts its result back later.
    #[serde(default)]
    pub background: bool,
    /// Policy when a *foreground* spawn exceeds its foreground wait: a
    /// user-facing session converts to background (default) or kills.
    /// Ignored for `background: true` and for non-user parents.
    #[serde(default)]
    pub on_timeout: OnTimeout,
    /// Root session id used by the dispatcher's fan-out limiter.
    /// Stamped by `spawn_subagent` after it walks the lineage chain
    /// (and reserves a slot in the limiter), so the router's wait
    /// task can release the slot on terminal without re-walking.
    /// `None` only on synthesized requests from tests / future
    /// programmatic call sites that don't go through the limiter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_out_root: Option<SessionId>,
    /// Backend that runs this subagent. `Baybo` is the default (full
    /// in-process `AgentActor`). `External` routes to a registered
    /// external-agent impl (claude_cli, …) for one-shot delegation.
    #[serde(default)]
    pub backend: SubagentBackend,
    /// Continue a prior subagent's session. The id MUST come from a
    /// previous `SubagentResult.child_session_id` in this same parent
    /// session. The router verifies parent + backend match before
    /// routing the new task into the existing child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_session_id: Option<SessionId>,
    /// Barrier cohort. When set, this subagent runs in the background and
    /// its result is held until every member of the group reaches a
    /// terminal state, then the whole group delivers in one merged
    /// notification. Tagged onto the escorted `PendingBackgroundResult`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// Parent-side context the tool builds from its `ToolContext` and passes to
/// the `SubagentSpawner` alongside the `SubagentSpawnRequest`.
#[derive(Debug, Clone)]
pub struct SubagentParentContext {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    /// Parent's `ToolCall(spawn_subagent)` span id — recorded on the
    /// child's `Lineage` so trace viewers can hop from the parent's
    /// tool span to the child session.
    pub span_id: SpanId,
    pub cancel_token: CancellationToken,
    /// Whether the dispatching turn may create background work — the gate
    /// the agent loop computed for this turn (see
    /// `ToolContext::background_eligible`), carried through the tool so the
    /// spawner does not re-derive it from the parent session alone and miss
    /// the turn half. `false` makes a foreground spawn block until terminal
    /// and downgrades an explicit `background: true` to a blocking run.
    pub background_eligible: bool,
    /// Transient tool authority inherited from the dispatching execution.
    /// `Some(default())` is distinct from `None` and keeps delegated work in
    /// the same fail-closed regime. The actor-backed Baybo spawner forwards it
    /// unchanged; external backends have no access to Baybo's tool registry.
    pub inherited_context: Option<InheritedToolContext>,
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
    Failed {
        reason: String,
    },
    Timeout,
}

/// Persistent record of a background job — a `background` (or
/// timeout-converted) subagent, or a detached `Bash` command — that
/// reached a terminal state while the parent actor was between turns.
/// Held in [`crate::BackgroundNotificationState::buffered_results`] until
/// drained into a notification turn.
///
/// `handle_id` is the synthetic identifier the spawning tool minted and
/// surfaced to the parent LLM as the "dispatched" handle, so a later
/// notification can name the same id the parent saw at dispatch time.
/// `label` is a short display title (the subagent task summary, or the
/// command line); `summary_text` is the result body carried into the
/// **LLM-only** notification prompt (the subagent's report, or the command's
/// output tail) — the user never sees it directly, only the parent's analysed
/// report of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingBackgroundResult {
    pub handle_id: String,
    pub label: String,
    pub summary_text: String,
    pub status: SubagentExitStatus,
    pub kind: BackgroundJobKind,
    /// Barrier cohort this result belongs to, if the spawning subagent
    /// carried a `group`. Grouped results are held in the buffer until
    /// their group is ready (sealed + all members terminal) or its group
    /// timeout dissolves it. `None` for ungrouped jobs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// Which kind of background job a [`PendingBackgroundResult`] reports.
///
/// The tag key stays `job`: this is a **background job**, not the turn entity,
/// and the tag is a persisted discriminator. `PendingBackgroundResult` rides
/// inside `Session.background_notifications.pending_background_results`, so
/// retagging it would make any session holding a buffered result fail to
/// deserialize whole — and the session-list decoder drops undecodable rows with
/// a warning, which would take a live conversation out of the user's list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum BackgroundJobKind {
    /// A `background` (or timeout-converted) subagent.
    Subagent {
        child_session_id: SessionId,
        subagent_type: String,
    },
    /// A detached `Bash` command. `output_path` is where the full output
    /// streamed; the agent `Read`s it for detail beyond `summary_text`.
    Command {
        command: String,
        exit_code: i32,
        output_path: String,
    },
}

impl PendingBackgroundResult {
    /// Build a result for a finished background subagent.
    pub fn subagent(
        handle_id: impl Into<String>,
        subagent_type: impl Into<String>,
        task_summary: impl Into<String>,
        child_session_id: SessionId,
        final_text: impl Into<String>,
        status: SubagentExitStatus,
    ) -> Self {
        Self {
            handle_id: handle_id.into(),
            label: task_summary.into(),
            summary_text: final_text.into(),
            status,
            kind: BackgroundJobKind::Subagent {
                child_session_id,
                subagent_type: subagent_type.into(),
            },
            group: None,
        }
    }

    /// Build a result for a finished detached command.
    pub fn command(
        handle_id: impl Into<String>,
        command: impl Into<String>,
        exit_code: i32,
        output_path: impl Into<String>,
        output_tail: impl Into<String>,
        status: SubagentExitStatus,
    ) -> Self {
        let command = command.into();
        Self {
            handle_id: handle_id.into(),
            label: command.clone(),
            summary_text: output_tail.into(),
            status,
            kind: BackgroundJobKind::Command {
                command,
                exit_code,
                output_path: output_path.into(),
            },
            group: None,
        }
    }
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

    /// The parent-visible text for this result's `tool_result` content
    /// (without the resume tail). Always non-empty. Non-text blocks
    /// (thinking, tool_use, images) are dropped — they're internal
    /// subagent state; the parent gets the subagent's textual report.
    pub fn result_text(&self) -> String {
        match (&self.status, &self.final_content) {
            (SubagentExitStatus::Completed, Some(blocks)) => {
                let text = extract_text(blocks);
                if text.is_empty() {
                    "[subagent completed without producing a final message]".to_string()
                } else {
                    text
                }
            }
            (SubagentExitStatus::Completed, None) => {
                "[subagent completed without producing a final message]".to_string()
            }
            (SubagentExitStatus::Cancelled, _) => {
                "[subagent cancelled by parent before producing a result]".to_string()
            }
            (SubagentExitStatus::Failed { reason }, _) => format!("[subagent failed: {reason}]"),
            (SubagentExitStatus::Timeout, _) => {
                "[subagent idle timeout — produced no output within the safety window]".to_string()
            }
        }
    }

    /// The parseable `[subagent_session_id: …]` suffix appended to a
    /// completed result's text so the parent can continue the child via
    /// `spawn_subagent(resume_session_id: …)`. `None` for non-completed
    /// exits or early failures that never minted a child session. Single
    /// source of truth for the tail format — the tool boundary applies
    /// it to its already-split text rather than re-rendering.
    pub fn resume_tail(&self) -> Option<String> {
        let id = self.child_session_id.as_ref();
        (matches!(self.status, SubagentExitStatus::Completed) && !id.is_empty())
            .then(|| format!("\n[subagent_session_id: {id}]"))
    }

    /// Flat text rendering for the parent's synthetic `tool_result`:
    /// [`Self::result_text`] plus the [`Self::resume_tail`] suffix on
    /// completion. Callers that want the body without the tail (e.g. the
    /// background ack) use [`Self::result_text`] directly.
    pub fn to_tool_result_text(&self) -> String {
        let mut text = self.result_text();
        if let Some(tail) = self.resume_tail() {
            text.push_str(&tail);
        }
        text
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

    #[test]
    fn tool_result_text_completed_concatenates_text_blocks_and_tails_session_id() {
        let r = SubagentResult {
            child_session_id: SessionId::from("child-1"),
            final_content: Some(vec![
                ContentBlock::Text("line one".into()),
                ContentBlock::Text("line two".into()),
            ]),
            status: SubagentExitStatus::Completed,
        };
        let text = r.to_tool_result_text();
        assert!(text.starts_with("line one\nline two"), "got: {text}");
        assert!(
            text.contains("[subagent_session_id: child-1]"),
            "missing session-id tail: {text}",
        );
    }

    #[test]
    fn tool_result_text_failure_does_not_tail_session_id() {
        let r = SubagentResult::failed("boom");
        let text = r.to_tool_result_text();
        assert!(text.contains("boom"));
        assert!(
            !text.contains("subagent_session_id"),
            "failed result should not advertise a resume id: {text}",
        );
    }

    #[test]
    fn result_text_keeps_only_text_blocks() {
        let r = SubagentResult {
            child_session_id: SessionId::from("child-1"),
            final_content: Some(vec![
                ContentBlock::Text("a".into()),
                ContentBlock::Image {
                    blob: crate::BlobRef {
                        blob_id: "b-1".into(),
                    },
                    mime_type: "image/png".into(),
                    filename: None,
                    width: None,
                    height: None,
                },
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
        // Only text reaches the parent — images / thinking / tool_use dropped.
        assert_eq!(r.result_text(), "a");
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

    /// The persisted tag key for a background job is `job`, not `turn`.
    ///
    /// `PendingBackgroundResult` is serialized inside `Session`, so a retag
    /// makes every session holding a buffered background result undecodable —
    /// and the session-list decoder drops such rows with a warning, silently
    /// removing a live conversation from the user's list. Pinned against a
    /// literal blob rather than a round-trip, because a round-trip agrees with
    /// itself no matter which key it uses.
    #[test]
    fn background_job_kind_keeps_its_persisted_tag() {
        let stored = r#"{"job":"subagent","child_session_id":"c1","subagent_type":"researcher"}"#;
        let decoded: BackgroundJobKind =
            serde_json::from_str(stored).expect("a session persisted before the Turn rename");
        assert!(matches!(decoded, BackgroundJobKind::Subagent { .. }));

        let reserialized = serde_json::to_value(&decoded).expect("serialize");
        assert_eq!(
            reserialized.get("job").and_then(|v| v.as_str()),
            Some("subagent")
        );
        assert!(reserialized.get("turn").is_none());
    }
}
