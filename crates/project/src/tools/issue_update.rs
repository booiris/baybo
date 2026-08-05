use std::sync::Arc;

use async_trait::async_trait;
use baybo_store::project::IssueUpdate;
use baybo_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    actor, exec_err, parse_priority, parse_status, priority_schema, project_err, render_issue,
    resolve_handle, scope, status_schema,
};
use crate::ProjectManager;

pub const ISSUE_UPDATE_TOOL_NAME: &str = "IssueUpdate";

/// The `assignee` value that takes somebody off a card.
const UNASSIGN: &str = "none";

pub(super) struct IssueUpdateTool {
    manager: Arc<ProjectManager>,
}

impl IssueUpdateTool {
    pub(super) fn new(manager: Arc<ProjectManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    number: i64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    /// An `@handle`, or `none` to unassign.
    #[serde(default)]
    assignee: Option<String>,
    /// A reason to block on, or an empty string to unblock.
    #[serde(default)]
    blocked: Option<String>,
    #[serde(default)]
    cancelled: Option<bool>,
}

impl IssueUpdateTool {
    /// The destination column's contents with `number` appended.
    ///
    /// A move rewrites the whole target column's order, so the tool has to
    /// say where the card lands. The end of the column is the honest answer:
    /// an agent moving a card is saying it belongs in that state, not that
    /// it outranks everything already there.
    async fn column_order(
        &self,
        project: &baybo_model::ProjectId,
        status: baybo_store::project::IssueStatus,
        number: i64,
    ) -> baybo_tools::Result<Vec<i64>> {
        let mut column: Vec<_> = self
            .manager
            .list_issues(project)
            .await
            .map_err(exec_err)?
            .into_iter()
            .filter(|issue| issue.status == status && issue.number != number)
            .collect();
        column.sort_by_key(|issue| issue.position);
        let mut order: Vec<i64> = column.into_iter().map(|issue| issue.number).collect();
        order.push(number);
        Ok(order)
    }
}

#[async_trait]
impl Tool for IssueUpdateTool {
    fn name(&self) -> &str {
        ISSUE_UPDATE_TOOL_NAME
    }

    fn description(&self) -> String {
        format!(
            r#"Change one issue on this project's board. Only the fields you name are touched.

Two of these do more than edit a row:
- Moving an issue to `in_progress` with an assignee **starts that agent**. Move work there when it is being done now, not when it is next.
- Setting `blocked` to a reason marks the card blocked and says why on its timeline; an empty string unblocks it.

`assignee` takes an `@handle`, or `{UNASSIGN}` to take whoever is on it off. `cancelled: true` is the terminal negative — the row and its history stay, it just stops counting as live work. Say why in a comment before you cancel somebody else's card."#
        )
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "number": { "type": "integer", "description": "The issue's number on this board." },
                "title": { "type": "string" },
                "description": { "type": "string" },
                "status": status_schema("Move it to this column. `in_progress` with an assignee starts a run."),
                "priority": priority_schema("Informs triage."),
                "assignee": { "type": "string", "description": format!("An `@handle` from the team, or `{UNASSIGN}` to unassign.") },
                "blocked": { "type": "string", "description": "Why it is blocked; an empty string unblocks it." },
                "cancelled": { "type": "boolean", "description": "Cancel (or un-cancel) the issue." },
            },
            "required": ["number"],
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("number")
            .and_then(Value::as_i64)
            .map(|n| format!("#{n}"))
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::ProjectBoard
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let project = scope(ctx)?;
        let assignee = match p.assignee.as_deref().map(str::trim) {
            None => None,
            Some(raw) if raw.is_empty() || raw.eq_ignore_ascii_case(UNASSIGN) => Some(None),
            Some(raw) => Some(Some(resolve_handle(&self.manager, &project, raw).await?)),
        };
        let status = p.status.as_deref().map(parse_status).transpose()?;
        let update = IssueUpdate {
            title: p.title,
            description: p.description,
            priority: p.priority.as_deref().map(parse_priority).transpose()?,
            assignee,
            // An all-whitespace reason is an unblock, which the manager
            // already normalises — passing it through keeps the two callers
            // agreeing on what an empty string means.
            blocked_reason: p.blocked.map(Some),
            cancelled: p.cancelled,
        };
        if update.is_empty() && status.is_none() {
            return Err(ToolError::InvalidParams(
                "name at least one field to change".to_owned(),
            ));
        }

        // Fields first, column second. A card that arrives in In Progress
        // already carrying its assignee starts on that move; the other
        // order refuses it — In Progress needs somebody on it — and would
        // make "assign and start" two calls instead of one.
        let mut issue = if update.is_empty() {
            self.manager
                .get_issue(&project, p.number)
                .await
                .map_err(project_err)?
        } else {
            self.manager
                .update_issue(&project, p.number, actor(ctx), update)
                .await
                .map_err(project_err)?
        };
        if let Some(status) = status.filter(|status| *status != issue.status) {
            let order = self.column_order(&project, status, p.number).await?;
            issue = self
                .manager
                .move_issue(&project, p.number, actor(ctx), status, &order)
                .await
                .map_err(project_err)?;
        }
        let team = self.manager.team(&project).await.map_err(exec_err)?;
        Ok(ToolOutput::Json(render_issue(&issue, &team)))
    }
}
