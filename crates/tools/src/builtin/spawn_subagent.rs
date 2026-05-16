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
    MAX_SUBAGENT_TIMEOUT_SECS, SPAWN_SUBAGENT_TOOL_NAME, SubagentParentContext, SubagentResult,
    SystemSpawnRequest, parse_spawn_request,
};
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
            "llm": {
                "type": "string",
                "description": "Optional LLM entry-name override for the spawned child (must match an entry in aura.json `llm[*].name`)."
            }
        }
    })
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        SPAWN_SUBAGENT_TOOL_NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
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
            ToolOutput::Text(s) => assert_eq!(s, "done"),
            _ => panic!("expected Text output"),
        }
        let (req, parent_id) = router.await.unwrap().expect("router saw a Subagent frame");
        assert_eq!(req.task_description, "look up X");
        assert_eq!(req.timeout, Duration::from_secs(30));
        assert_eq!(req.llm.as_deref(), Some("fast"));
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
}
