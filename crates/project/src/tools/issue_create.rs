use std::sync::Arc;

use async_trait::async_trait;
use baybo_store::project::{IssuePriority, IssueStatus};
use baybo_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    actor, exec_err, parse_priority, parse_status, priority_schema, project_err, render_issue,
    resolve_handle, scope, status_schema,
};
use crate::{NewIssueRequest, ProjectManager};

pub const ISSUE_CREATE_TOOL_NAME: &str = "IssueCreate";

pub(super) struct IssueCreateTool {
    manager: Arc<ProjectManager>,
}

impl IssueCreateTool {
    pub(super) fn new(manager: Arc<ProjectManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    /// Make it a step of this issue's number.
    #[serde(default)]
    parent: Option<i64>,
    #[serde(default)]
    stage: i64,
}

#[async_trait]
impl Tool for IssueCreateTool {
    fn name(&self) -> &str {
        ISSUE_CREATE_TOOL_NAME
    }

    fn description(&self) -> String {
        r#"Open a new issue on this project's board. Give it a title somebody can act on and a description carrying what you know — the assignee reads only what is on the card. `status` defaults to `backlog`; `assignee` is an `@handle` from the team.

Putting an issue straight into `in_progress` with an assignee starts that agent immediately, so do it only when you mean "work this now". `in_progress` without an assignee is refused: it would claim work nobody is doing."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "One actionable line. It is what shows on the card." },
                "description": { "type": "string", "description": "What the assignee needs to know: context, constraints, what done looks like." },
                "status": status_schema("Which column it opens in. Defaults to `backlog`."),
                "priority": priority_schema("Informs triage; it never reorders the board on its own."),
                "assignee": { "type": "string", "description": "An `@handle` from this project's team." },
                "parent": { "type": "integer", "description": "Open it as a step of that issue's number. One level only — a step cannot have steps." },
                "stage": { "type": "integer", "description": "Which barrier under the parent (default 0). Stage N starts when every step of stage N-1 is done." },
            },
            "required": ["title"],
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("title")
            .and_then(Value::as_str)
            .and_then(baybo_tools::progress::preview_arg)
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::ProjectBoard
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let project = scope(ctx)?;
        let assignee = match p.assignee.as_deref() {
            Some(handle) => Some(resolve_handle(&self.manager, &project, handle).await?),
            None => None,
        };
        let issue = self
            .manager
            .create_issue(
                &project,
                actor(ctx),
                NewIssueRequest {
                    title: p.title,
                    description: p.description,
                    status: p
                        .status
                        .as_deref()
                        .map(parse_status)
                        .transpose()?
                        .unwrap_or(IssueStatus::Backlog),
                    priority: p
                        .priority
                        .as_deref()
                        .map(parse_priority)
                        .transpose()?
                        .unwrap_or(IssuePriority::None),
                    assignee,
                    parent: p.parent,
                    stage: p.stage,
                },
            )
            .await
            .map_err(project_err)?;
        let team = self.manager.team(&project).await.map_err(exec_err)?;
        Ok(ToolOutput::Json(render_issue(&issue, &team)))
    }
}
