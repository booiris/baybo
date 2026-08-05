use std::sync::Arc;

use async_trait::async_trait;
use baybo_store::project::{IssuePriority, IssueStatus};
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{exec_err, parse_status, render_issue, scope, status_schema};
use crate::ProjectManager;

pub const ISSUE_LIST_TOOL_NAME: &str = "IssueList";

pub(super) struct IssueListTool {
    manager: Arc<ProjectManager>,
}

impl IssueListTool {
    pub(super) fn new(manager: Arc<ProjectManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Default, Deserialize)]
struct Params {
    #[serde(default)]
    status: Option<String>,
    /// `@handle`, or the literal `unassigned`.
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    include_cancelled: bool,
}

/// The `assignee` filter value that means "nobody is on it" — the set the
/// lead triages. A sentinel rather than a second boolean parameter, because
/// "assigned to nobody" is a value of the same field, not a different
/// question.
const UNASSIGNED: &str = "unassigned";

#[async_trait]
impl Tool for IssueListTool {
    fn name(&self) -> &str {
        ISSUE_LIST_TOOL_NAME
    }

    fn description(&self) -> String {
        format!(
            r#"List the issues on this project's board. Returns each card's number, title, status, priority, assignee handle, and branch if it has produced one. Filter with `status` (one column) and `assignee` (an `@handle`, or `{UNASSIGNED}` for the cards nobody has picked up — that set is what triage is about). Cancelled issues are left out unless you ask for them. Also returns the project's team, so you can see who is available before assigning anything."#
        )
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": status_schema("Only issues in this column."),
                "assignee": {
                    "type": "string",
                    "description": format!("Only issues assigned to this `@handle`, or `{UNASSIGNED}` for the ones nobody is on."),
                },
                "include_cancelled": {
                    "type": "boolean",
                    "description": "Include cancelled issues. Default false — they are not live work.",
                },
            },
        })
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::ProjectBoard
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: Params = if params.is_null() {
            Params::default()
        } else {
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?
        };
        let project = scope(ctx)?;
        let status = p.status.as_deref().map(parse_status).transpose()?;

        let team = self.manager.team(&project).await.map_err(exec_err)?;
        let wanted_assignee = match p.assignee.as_deref().map(str::trim) {
            None => None,
            Some(raw) if raw.eq_ignore_ascii_case(UNASSIGNED) => Some(None),
            Some(raw) => Some(Some(
                super::resolve_handle(&self.manager, &project, raw).await?,
            )),
        };

        let mut issues = self.manager.list_issues(&project).await.map_err(exec_err)?;
        issues.retain(|issue| {
            (p.include_cancelled || issue.cancelled_at.is_none())
                && status.is_none_or(|s| issue.status == s)
                && wanted_assignee
                    .as_ref()
                    .is_none_or(|wanted| &issue.assignee == wanted)
        });
        // Most urgent first inside each column, so a triage read does not
        // have to sort a hundred rows itself. `IssueStatus::ALL` is board
        // order and `IssuePriority::ALL` is urgency order, so deriving the
        // keys from them keeps this from drifting.
        issues.sort_by_key(|issue| {
            (
                IssueStatus::ALL.iter().position(|s| *s == issue.status),
                IssuePriority::ALL.iter().position(|p| *p == issue.priority),
                issue.position,
            )
        });

        let roster: Vec<Value> = team
            .iter()
            .filter_map(|row| {
                row.team
                    .as_ref()
                    .map(|t| json!({ "handle": format!("@{}", t.handle), "role": row.description }))
            })
            .collect();
        Ok(ToolOutput::Json(json!({
            "count": issues.len(),
            "issues": issues.iter().map(|i| render_issue(i, &team)).collect::<Vec<_>>(),
            "team": roster,
        })))
    }
}
