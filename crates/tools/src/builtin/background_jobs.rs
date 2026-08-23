//! `JobList` / `JobStop` — view and control the in-flight background jobs
//! (detached subagents and `Bash` commands) of the current conversation.
//! Backed by [`crate::BackgroundJobControl`]; where no manager is wired the
//! tools simply report nothing in flight. Observing existing jobs is not
//! creating work, so neither tool consults
//! [`crate::ToolContext::background_eligible`] — a scheduled job's own run
//! can still see and stop what a reply in the same conversation started.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolError, ToolOutput};

pub struct JobListTool;

#[async_trait]
impl Tool for JobListTool {
    fn trigger_scope(&self) -> crate::ToolTriggerScope {
        crate::ToolTriggerScope::BackgroundHost
    }

    fn name(&self) -> &str {
        "JobList"
    }

    fn description(&self) -> String {
        "List the background jobs (detached subagents and Bash commands) still \
         running for this conversation. Each entry has a handle, a kind, and a \
         summary. A detached command's output streams to \
         logs/background/<handle>.out (and .err) — Read it for live progress. \
         Returns an empty list when nothing is in flight."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let jobs = match &ctx.background_control {
            Some(c) => c.list(&ctx.session_id).await,
            None => Vec::new(),
        };
        let items: Vec<Value> = jobs
            .into_iter()
            .map(|j| json!({ "handle": j.handle, "kind": j.kind, "summary": j.summary }))
            .collect();
        Ok(ToolOutput::Json(json!({ "jobs": items })))
    }
}

#[derive(Debug, Deserialize)]
struct JobStopParams {
    handle: String,
}

pub struct JobStopTool;

#[async_trait]
impl Tool for JobStopTool {
    fn trigger_scope(&self) -> crate::ToolTriggerScope {
        crate::ToolTriggerScope::BackgroundHost
    }

    fn name(&self) -> &str {
        "JobStop"
    }

    fn description(&self) -> String {
        "Kill one in-flight background job by its handle (from JobList, or the \
         notice you got when it was backgrounded). The job is terminated and \
         will NOT send a completion notification. Returns whether a matching \
         running job was found."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "handle": { "type": "string", "description": "The job handle to stop (e.g. bg-…)." }
            },
            "required": ["handle"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: JobStopParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let stopped = match &ctx.background_control {
            Some(c) => c.stop(&ctx.session_id, &p.handle).await,
            None => false,
        };
        let text = if stopped {
            format!("Stopped background job `{}`.", p.handle)
        } else {
            format!("No in-flight background job with handle `{}`.", p.handle)
        };
        Ok(ToolOutput::Text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackgroundJobControl, BackgroundJobInfo};
    use baybo_model::{ChannelType, SessionId, User};
    use std::sync::Arc;
    use std::time::Duration;

    struct FakeControl;

    #[async_trait]
    impl BackgroundJobControl for FakeControl {
        async fn list(&self, _session_id: &SessionId) -> Vec<BackgroundJobInfo> {
            vec![BackgroundJobInfo {
                handle: "bg-1".into(),
                kind: "command".into(),
                summary: "sleep 30".into(),
            }]
        }
        async fn stop(&self, _session_id: &SessionId, handle: &str) -> bool {
            handle == "bg-1"
        }
    }

    fn ctx(control: Option<Arc<dyn BackgroundJobControl>>) -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(1),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            background_control: control,
            ..ToolContext::for_test()
        }
    }

    #[tokio::test]
    async fn job_list_is_empty_without_a_control() {
        let out = JobListTool.execute(json!({}), &ctx(None)).await.unwrap();
        let ToolOutput::Json(v) = out else {
            panic!("expected json");
        };
        assert_eq!(v["jobs"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn job_list_reports_in_flight_jobs() {
        let out = JobListTool
            .execute(json!({}), &ctx(Some(Arc::new(FakeControl))))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else {
            panic!("expected json");
        };
        assert_eq!(v["jobs"][0]["handle"], "bg-1");
        assert_eq!(v["jobs"][0]["kind"], "command");
        assert_eq!(v["jobs"][0]["summary"], "sleep 30");
    }

    #[tokio::test]
    async fn job_stop_reports_found_and_missing() {
        let found = JobStopTool
            .execute(
                json!({ "handle": "bg-1" }),
                &ctx(Some(Arc::new(FakeControl))),
            )
            .await
            .unwrap();
        let ToolOutput::Text(t) = found else {
            panic!("expected text");
        };
        assert!(t.contains("Stopped"), "{t}");

        let missing = JobStopTool
            .execute(
                json!({ "handle": "nope" }),
                &ctx(Some(Arc::new(FakeControl))),
            )
            .await
            .unwrap();
        let ToolOutput::Text(t) = missing else {
            panic!("expected text");
        };
        assert!(t.contains("No in-flight"), "{t}");
    }
}
