use std::sync::Arc;

use async_trait::async_trait;
use baybo_store::project::IssueUpdate;
use baybo_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    NOBODY_WORD, actor, exec_err, parse_priority, parse_status, project_err, render_issue, scope,
};
use crate::{Placement, ProjectManager};

pub const ISSUE_UPDATE_TOOL_NAME: &str = "IssueUpdate";

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
    clear_description: bool,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    /// An `@handle`, or `none` to unassign.
    #[serde(default)]
    assignee: Option<String>,
    /// A reason to block on. Unblocking has its own explicit flag because an
    /// empty string is the strict-schema filler.
    #[serde(default)]
    blocked: Option<String>,
    #[serde(default)]
    unblock: bool,
    #[serde(default)]
    cancelled: Option<bool>,
    #[serde(default)]
    reopen: bool,
    /// The issue this one is a step of, by number. Never a detach — see
    /// [`Placement`].
    #[serde(default)]
    parent: Option<i64>,
    /// Lands only alongside `parent` or `detach_parent`.
    #[serde(default)]
    stage: Option<i64>,
    /// Take this card out of its parent's plan. The only way to do that.
    #[serde(default)]
    detach_parent: bool,
}

impl IssueUpdateTool {
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
- Setting `blocked` to a reason marks the card blocked and says why on its timeline; `unblock: true` lifts it.

`assignee` takes an `@handle`, or `{NOBODY_WORD}` to take whoever is on it off. `cancelled: true` is the terminal negative — the row and its history stay, it just stops counting as live work. Say why in a comment before you cancel somebody else's card, and when the operator cancelled one, say on the card why it should come back rather than reopening it yourself."#
        )
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "number": { "type": "integer", "description": "The issue's number on this board." },
                "title": { "type": "string", "description": "New title. An empty string means unchanged." },
                "description": { "type": "string", "description": "New description. An empty string means unchanged; use `clear_description` to remove it." },
                "clear_description": { "type": "boolean", "description": "Set true to clear the description. False is a no-op." },
                "status": super::status_update_schema("Move it to this column. `in_progress` with an assignee starts a run. Use `unchanged` when the strict schema requires a value but the column is not changing."),
                "priority": super::priority_update_schema("Informs triage. Use `unchanged` when the strict schema requires a value but priority is not changing."),
                "assignee": super::assignee_schema(true),
                "blocked": { "type": "string", "description": "Why it is blocked. An empty string means unchanged; use `unblock` to lift the block." },
                "unblock": { "type": "boolean", "description": "Set true to unblock the issue. False is a no-op." },
                "cancelled": { "type": "boolean", "description": "Set true to cancel the issue. False is a no-op; use `reopen` to take back a cancel the board itself set." },
                "reopen": { "type": "boolean", "description": "Set true to take back a cancel the board itself set. One the operator set is theirs to lift. False is a no-op." },
                "parent": { "type": "integer", "description": "Make this a sub-issue of that issue's number. One level only. Never detaches — use `detach_parent`." },
                "stage": { "type": "integer", "description": "Which barrier under the parent. Stage N starts when every step of stage N-1 is done, and the board holds a step in Todo until then; move it to `in_progress` yourself to start it sooner. Send it together with `parent` — alone it is ignored, so pass the card's current stage when you are not moving it." },
                "detach_parent": { "type": "boolean", "description": "`true` takes this card out of its parent's plan and clears its stage." },
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
        let assignee = super::parse_assignee_value(
            &self.manager,
            &project,
            p.assignee
                .as_deref()
                .map(str::trim)
                .filter(|assignee| !assignee.is_empty()),
        )
        .await?;
        let status = p
            .status
            .as_deref()
            .map(str::trim)
            .filter(|status| !status.is_empty() && *status != super::UNCHANGED_ACTION)
            .map(parse_status)
            .transpose()?;
        let priority = p
            .priority
            .as_deref()
            .map(str::trim)
            .filter(|priority| !priority.is_empty() && *priority != super::UNCHANGED_ACTION)
            .map(parse_priority)
            .transpose()?;
        let description = p.description.filter(|value| !value.trim().is_empty());
        if p.clear_description && description.is_some() {
            return Err(ToolError::InvalidParams(
                "`description` and `clear_description: true` are mutually exclusive".into(),
            ));
        }
        let blocked = p.blocked.filter(|reason| !reason.trim().is_empty());
        if p.unblock && blocked.is_some() {
            return Err(ToolError::InvalidParams(
                "`blocked` and `unblock: true` are mutually exclusive".into(),
            ));
        }
        if p.reopen && p.cancelled == Some(true) {
            return Err(ToolError::InvalidParams(
                "`cancelled: true` and `reopen: true` are mutually exclusive".into(),
            ));
        }
        // The model addresses the parent by number, like everything else on
        // the board; the store wants an id. Resolved through the manager
        // rather than here: it keeps ULIDs out of the schema, and it keeps
        // one home for what a number below `1` means — this tool is where a
        // filler value proved it needed one.
        let (parent, stage) = self
            .manager
            .resolve_placement(
                &project,
                Placement {
                    parent: p.parent,
                    detach: p.detach_parent,
                    stage: p.stage,
                },
            )
            .await
            .map_err(project_err)?;
        let update = IssueUpdate {
            title: p.title.filter(|value| !value.trim().is_empty()),
            description: description.or_else(|| p.clear_description.then(String::new)),
            // The manager fills this from blob ids, and this tool takes
            // none: an agent hangs files on a comment, where the timeline
            // records who put them there and why.
            attachments: None,
            parent,
            stage,
            priority,
            assignee,
            blocked_reason: blocked.map(Some).or_else(|| p.unblock.then_some(None)),
            cancelled: p
                .cancelled
                .filter(|cancelled| *cancelled)
                .or_else(|| p.reopen.then_some(false)),
            // Not in this tool's vocabulary, deliberately. The pin is the
            // operator's own reading order — an agent that could set it
            // would be reaching into how its work is looked at rather than
            // changing anything about the work.
            pinned: None,
        };
        if update.is_empty() && status.is_none() {
            return Err(ToolError::InvalidParams(
                "name at least one field to change".to_owned(),
            ));
        }

        let mut issue = if update.is_empty() {
            self.manager
                .get_issue(&project, p.number)
                .await
                .map_err(project_err)?
        } else {
            self.manager
                .update_issue(&project, p.number, actor(ctx), update, None)
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
