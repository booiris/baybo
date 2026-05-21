//! `spawn_subagent` builtin. Blocking tool: ships a
//! [`SystemSpawnRequest::Subagent`] envelope onto the agent runtime's
//! system-spawn channel and waits on the oneshot for the child's
//! terminal [`SubagentResult`].
//!
//! Registered by the runtime wiring code (not [`crate::builtin::default_tools`])
//! because it needs the runtime-owned `system_spawn_tx` sender.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_model::{
    AURA_BACKEND_TAG, ExternalAgentKind, MAX_SUBAGENT_TIMEOUT_SECS, SPAWN_SUBAGENT_TOOL_NAME,
    SessionId, SubagentBackend, SubagentParentContext, SubagentResult, SubagentSpawnRequest,
    SystemSpawnRequest,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::builtin::trusted;
use crate::{Tool, ToolContext, ToolError, ToolManifest, ToolOutput};

const DESCRIPTION: &str = r#"Spawn a child subagent to investigate or perform a focused sub-task.
The subagent runs as an independent session with its own transcript and tool budget.
This call blocks until the subagent returns its final message (or hits its declared timeout).
Use sparingly — each spawn incurs a fresh LLM cost stream."#;

/// Mirrors [`aura_model::MAX_SUBAGENT_TIMEOUT_SECS`] (single source of truth).
const MAX_OUTER_TIMEOUT: Duration = Duration::from_secs(MAX_SUBAGENT_TIMEOUT_SECS);

const DEFAULT_SUBAGENT_TIMEOUT_SECS: u64 = 600;

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
    /// `"aura"` (default), `"claude"`, or `"codex"`.
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    llm: Option<String>,
    #[serde(default)]
    workspace_name: Option<String>,
    #[serde(default)]
    resume_session_id: Option<String>,
}

const MAX_WORKSPACE_NAME_BASE_LEN: usize = 32;

fn parse_spawn_request(value: &Value) -> Result<SubagentSpawnRequest, String> {
    let p: SpawnParams = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let secs = p
        .timeout_secs
        .unwrap_or(DEFAULT_SUBAGENT_TIMEOUT_SECS)
        .clamp(1, MAX_SUBAGENT_TIMEOUT_SECS);
    let workspace_name = p
        .workspace_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(sanitize_workspace_name);
    let llm = p.llm.filter(|s| !s.trim().is_empty());
    let backend = match p.backend.as_deref().map(str::trim) {
        None | Some("") => SubagentBackend::Aura { llm },
        Some(t) if t == AURA_BACKEND_TAG => SubagentBackend::Aura { llm },
        Some(other) => {
            if let Some(kind) = ExternalAgentKind::parse(other) {
                if llm.is_some() {
                    return Err(format!(
                        "`llm` is not valid when backend={other:?}; external backends have no \
                         LLM-entry choice"
                    ));
                }
                SubagentBackend::External {
                    external_kind: kind,
                }
            } else {
                return Err(format!("unknown backend {other:?}"));
            }
        }
    };
    let resume_session_id = p
        .resume_session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(SessionId::from);
    if resume_session_id.is_some() && workspace_name.is_some() {
        return Err(
            "`workspace_name` is fixed at the genesis spawn — drop it from resume calls; the \
             original dir is reused automatically"
                .into(),
        );
    }
    Ok(SubagentSpawnRequest {
        task_description: p.task_description,
        must_include_context: p.must_include_context,
        timeout: Duration::from_secs(secs),
        backend,
        workspace_name,
        resume_session_id,
    })
}

/// Lowercase + kebab-case `[a-z0-9-]`, truncated to
/// `MAX_WORKSPACE_NAME_BASE_LEN`. Two spawns with the same input
/// share the same on-disk dir on purpose — letting a sibling
/// subagent see the prior one's files is the legitimate reason to
/// pass an explicit `workspace_name`.
fn sanitize_workspace_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_was_dash = true;
    for ch in raw.chars() {
        let lowered = ch.to_ascii_lowercase();
        let keep = lowered.is_ascii_lowercase() || lowered.is_ascii_digit();
        if keep {
            out.push(lowered);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("subagent");
    }
    if out.len() > MAX_WORKSPACE_NAME_BASE_LEN {
        out.truncate(MAX_WORKSPACE_NAME_BASE_LEN);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

pub struct SpawnSubagentTool {
    system_spawn_tx: mpsc::Sender<SystemSpawnRequest>,
}

impl SpawnSubagentTool {
    pub fn new(system_spawn_tx: mpsc::Sender<SystemSpawnRequest>) -> Self {
        Self { system_spawn_tx }
    }
}

fn parameters_schema() -> Value {
    json!({
        "type": "object",
        "required": ["task_description"],
        "properties": {
            "task_description": {
                "type": "string",
                "description": "Self-contained task statement for the subagent. The subagent does NOT see the parent's transcript — include every fact it needs."
            },
            "must_include_context": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional bullets of context the subagent must keep visible (span ids, facts, constraints)."
            },
            "timeout_secs": {
                "type": "integer",
                "minimum": 1,
                "description": "Hard wait limit in seconds. Defaults to 600."
            },
            "backend": {
                "type": "string",
                "enum": ["aura", "claude", "codex"],
                "description": "Backend that runs the subagent. 'aura' (default) spawns a full in-process aura agent that uses the configured LLMs/tools/skills. 'claude' delegates to a local Claude Code subprocess; 'codex' delegates to OpenAI's codex CLI — both are one-shot, run their own internal tool loops with bypassed permissions, and are best for heavy autonomous tasks where you want claude/codex (not aura) to drive."
            },
            "llm": {
                "type": "string",
                "description": "Only valid when backend='aura'. Optional LLM entry-name override (must match an entry in aura.json `llm[*].name`)."
            },
            "workspace_name": {
                "type": "string",
                "description": "Optional short human-readable slug naming the subagent's working area (e.g. 'investigate-auth-bug', 'refactor-payment-flow'). Used by external-agent backends (claude, codex) so artifacts land under a meaningful path. Sanitised to kebab-case ASCII and capped at 32 chars. Two spawns with the same workspace_name share the same on-disk dir — pass the same name when you want a sibling subagent to pick up where another left off. Leave unset to fall back to the (unique) child session_id."
            },
            "resume_session_id": {
                "type": "string",
                "description": "Continue a prior subagent's conversation. Use a child_session_id value the user previously saw in a 'subagent_session_id: ...' tail on this same parent session — passing an arbitrary id will be rejected. The new task_description is appended to that child's existing context (for claude / codex, via the agent's own --resume). Do NOT also pass `workspace_name` — the original spawn's dir is reused automatically. Leave unset to start a fresh subagent."
            }
        }
    })
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        SPAWN_SUBAGENT_TOOL_NAME
    }

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters_schema(&self) -> Value {
        parameters_schema()
    }

    fn max_timeout(&self) -> Duration {
        MAX_OUTER_TIMEOUT
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let request = parse_spawn_request(&params).map_err(ToolError::InvalidParams)?;
        let parent = SubagentParentContext {
            session_id: ctx.session_id.clone(),
            job_id: ctx.job_id,
            span_id: ctx.span_id,
            cancel_token: ctx.cancellation_token.clone(),
        };
        let (result_tx, result_rx) = oneshot::channel();
        let envelope = SystemSpawnRequest::Subagent {
            parent_session_id: parent.session_id,
            parent_job_id: parent.job_id,
            parent_span_id: parent.span_id,
            parent_actor_token: parent.cancel_token,
            request,
            result_tx,
        };
        let result = if self.system_spawn_tx.send(envelope).await.is_err() {
            SubagentResult::failed("system spawn channel closed")
        } else {
            result_rx.await.unwrap_or_else(|_| {
                SubagentResult::failed("subagent result channel closed before delivery")
            })
        };
        Ok(ToolOutput::Text(result.to_tool_result_text()))
    }
}

pub fn make(system_spawn_tx: mpsc::Sender<SystemSpawnRequest>) -> (Arc<dyn Tool>, ToolManifest) {
    trusted(SpawnSubagentTool::new(system_spawn_tx), vec![])
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, SubagentExitStatus, SubagentSpawnRequest, User};
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "parent-sess".into(),
            job_id: aura_model::JobId::default(),
            span_id: aura_model::SpanId::default(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            cancellation_token: CancellationToken::new(),
            workspace_root: PathBuf::from("/tmp"),
            workspace_paths: aura_workspace::WorkspacePaths::new("/tmp"),
            sandbox: None,
            approval: None,
            notifier: None,
            events: crate::noop_event_sink(),
            llm: None,
        }
    }

    /// Spin a fake router: pull one `SystemSpawnRequest::Subagent` off
    /// the channel and reply on its oneshot with the supplied result.
    fn fake_router(
        mut rx: mpsc::Receiver<SystemSpawnRequest>,
        response: SubagentResult,
    ) -> tokio::task::JoinHandle<Option<(SubagentSpawnRequest, aura_model::SessionId)>> {
        tokio::spawn(async move {
            let envelope = rx.recv().await?;
            let SystemSpawnRequest::Subagent {
                request,
                parent_session_id,
                result_tx,
                ..
            } = envelope
            else {
                return None;
            };
            let _ = result_tx.send(response);
            Some((request, parent_session_id))
        })
    }

    #[tokio::test]
    async fn forwards_parsed_request_and_parent_ctx_to_channel() {
        let (tx, rx) = mpsc::channel::<SystemSpawnRequest>(8);
        let router = fake_router(
            rx,
            SubagentResult {
                child_session_id: aura_model::SessionId::from("child-1"),
                final_content: Some(vec![aura_model::ContentBlock::Text("done".into())]),
                status: SubagentExitStatus::Completed,
            },
        );

        let tool = SpawnSubagentTool::new(tx);
        let out = tool
            .execute(
                json!({
                    "task_description": "look up X",
                    "timeout_secs": 30,
                    "llm": "fast",
                }),
                &ctx(),
            )
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => {
                assert!(s.contains("done"), "got: {s}");
                assert!(
                    s.contains("[subagent_session_id: child-1]"),
                    "missing resume-id tail: {s}",
                );
            }
            _ => panic!("expected Text output"),
        }
        let (req, parent_id) = router.await.unwrap().expect("router saw a Subagent frame");
        assert_eq!(req.task_description, "look up X");
        assert_eq!(req.timeout, Duration::from_secs(30));
        match &req.backend {
            SubagentBackend::Aura { llm } => assert_eq!(llm.as_deref(), Some("fast")),
            other => panic!("expected Aura backend, got {other:?}"),
        }
        assert_eq!(parent_id.as_ref(), "parent-sess");
    }

    #[tokio::test]
    async fn invalid_params_surface_as_tool_error() {
        let (tx, _rx) = mpsc::channel::<SystemSpawnRequest>(1);
        let tool = SpawnSubagentTool::new(tx);
        let err = tool
            .execute(json!({"missing": "task_description"}), &ctx())
            .await
            .expect_err("missing task_description must error");
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn channel_closed_renders_failed_result() {
        let (tx, rx) = mpsc::channel::<SystemSpawnRequest>(1);
        drop(rx);
        let tool = SpawnSubagentTool::new(tx);
        let out = tool
            .execute(json!({"task_description": "x"}), &ctx())
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => assert!(s.contains("system spawn channel closed")),
            _ => panic!("expected Text output"),
        }
    }

    #[test]
    fn parse_spawn_request_minimal() {
        let v = json!({"task_description": "do the thing"});
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(req.task_description, "do the thing");
        assert_eq!(req.must_include_context.len(), 0);
        assert_eq!(
            req.timeout,
            Duration::from_secs(DEFAULT_SUBAGENT_TIMEOUT_SECS)
        );
        assert_eq!(req.backend, SubagentBackend::Aura { llm: None });
        assert!(req.workspace_name.is_none());
        assert!(req.resume_session_id.is_none());
    }

    #[test]
    fn parse_spawn_request_sanitises_workspace_name() {
        let v = json!({
            "task_description": "investigate",
            "workspace_name": "Investigate Auth Bug!!",
        });
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(req.workspace_name.as_deref(), Some("investigate-auth-bug"));
    }

    #[test]
    fn parse_spawn_request_same_workspace_name_yields_same_sanitized_output() {
        // Two spawns with the same workspace_name MUST resolve to the
        // same on-disk dir so a follow-up subagent can pick up where
        // the prior one left off. No uuid suffixes.
        let a = parse_spawn_request(&json!({
            "task_description": "first",
            "workspace_name": "shared-work",
        }))
        .unwrap();
        let b = parse_spawn_request(&json!({
            "task_description": "second",
            "workspace_name": "shared-work",
        }))
        .unwrap();
        assert_eq!(a.workspace_name, b.workspace_name);
    }

    #[test]
    fn parse_spawn_request_blank_workspace_name_is_treated_as_unset() {
        for blank in ["", "   ", "\t"] {
            let v = json!({"task_description": "x", "workspace_name": blank});
            let req = parse_spawn_request(&v).unwrap();
            assert_eq!(req.workspace_name, None, "blank {blank:?} should be None");
        }
    }

    #[test]
    fn sanitize_workspace_name_handles_punctuation_and_unicode() {
        assert_eq!(
            sanitize_workspace_name("Fix the BUG_123 (中文 case)"),
            "fix-the-bug-123-case",
        );
    }

    #[test]
    fn sanitize_workspace_name_truncates_long_input_at_cap() {
        let out = sanitize_workspace_name(
            "this-is-a-very-very-long-investigation-name-that-overflows-the-cap",
        );
        assert!(
            out.len() <= MAX_WORKSPACE_NAME_BASE_LEN,
            "len {} > cap {MAX_WORKSPACE_NAME_BASE_LEN}",
            out.len()
        );
        assert!(!out.ends_with('-'));
    }

    #[test]
    fn sanitize_workspace_name_empty_input_falls_back_to_subagent() {
        assert_eq!(sanitize_workspace_name("***"), "subagent");
    }

    #[test]
    fn sanitize_workspace_name_is_deterministic() {
        // Same input must produce the same output — sibling subagents
        // sharing a workspace_name share a dir.
        assert_eq!(
            sanitize_workspace_name("same-name"),
            sanitize_workspace_name("same-name"),
        );
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
        match req.backend {
            SubagentBackend::Aura { llm } => assert_eq!(llm.as_deref(), Some("fast")),
            other => panic!("expected Aura backend, got {other:?}"),
        }
    }

    #[test]
    fn parse_spawn_request_backend_claude() {
        let v = json!({
            "task_description": "investigate",
            "backend": "claude",
        });
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(
            req.backend,
            SubagentBackend::External {
                external_kind: ExternalAgentKind::Claude,
            }
        );
    }

    #[test]
    fn parse_spawn_request_backend_claude_rejects_llm() {
        // Strict shape: claude has no aura.json[llm] choice.
        let v = json!({
            "task_description": "x",
            "backend": "claude",
            "llm": "fast",
        });
        assert!(parse_spawn_request(&v).is_err());
    }

    #[test]
    fn parse_spawn_request_unknown_backend_rejected() {
        let v = json!({"task_description": "x", "backend": "nope"});
        assert!(parse_spawn_request(&v).is_err());
    }

    #[test]
    fn parse_spawn_request_with_resume_session_id() {
        let v = json!({
            "task_description": "follow-up",
            "resume_session_id": "child-abc",
        });
        let req = parse_spawn_request(&v).unwrap();
        assert_eq!(
            req.resume_session_id.as_ref().map(|s| s.as_ref()),
            Some("child-abc"),
        );
    }

    #[test]
    fn parse_spawn_request_rejects_workspace_name_on_resume() {
        // The genesis spawn's dir is durable on the child Session,
        // so resume calls can't choose a new one.
        let v = json!({
            "task_description": "follow-up",
            "resume_session_id": "child-abc",
            "workspace_name": "different-dir",
        });
        let err = parse_spawn_request(&v).expect_err("must reject");
        assert!(err.contains("workspace_name"), "got: {err}");
    }

    #[test]
    fn parse_spawn_request_rejects_missing_task() {
        let v = json!({"timeout_secs": 60});
        assert!(parse_spawn_request(&v).is_err());
    }

    #[test]
    fn parse_spawn_request_treats_blank_llm_as_unset() {
        for blank in ["", "   ", "\t"] {
            let v = json!({"task_description": "x", "llm": blank});
            let req = parse_spawn_request(&v).unwrap();
            match req.backend {
                SubagentBackend::Aura { llm } => assert_eq!(llm, None, "blank {blank:?}"),
                other => panic!("expected Aura, got {other:?}"),
            }
        }
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
}
