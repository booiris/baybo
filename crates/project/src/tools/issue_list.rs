use std::sync::Arc;

use async_trait::async_trait;
use baybo_store::project::{IssuePriority, IssueRow, IssueStatus};
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{exec_err, parse_status, render_issue, scope, status_schema, usd};
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
            r#"List the issues on this project's board. Returns each card's number, title, status, priority, assignee handle, and branch if it has produced one. Filter with `status` (one column) and `assignee` (an `@handle`, or `{UNASSIGNED}` for the cards nobody has picked up — that set is what triage is about). Cancelled issues are left out unless you ask for them.

Rows come back **most urgent first within each column**, so the order is already a triage order.

Alongside them: `team`, where each member's `working_on` is what they have in flight **right now** — which is not the same as which column a card sits in, because a run outlives the column it started in. `you` marks your own entry, so you know the handle to assign work to yourself. And `board`, which says what is held and what is left of today's budget: promoting a card on an exhausted board records a run that does not start."#
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
        // Every derived fact below is computed over the WHOLE board, never
        // the filtered rows. `assignee: unassigned` is the canonical triage
        // call, and a roster load derived from that set would always read
        // as an idle team.
        let load = self.manager.board_load(&project).await.map_err(exec_err)?;
        let board = self.manager.list_issues(&project).await.map_err(exec_err)?;
        let wanted_assignee = match p.assignee.as_deref().map(str::trim) {
            None => None,
            Some(raw) if raw.eq_ignore_ascii_case(UNASSIGNED) => Some(None),
            Some(raw) => Some(Some(
                super::resolve_handle(&self.manager, &project, raw).await?,
            )),
        };

        let mut issues = board.clone();
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
                let handle = row.team.as_ref()?;
                let mut entry = serde_json::Map::new();
                entry.insert("handle".into(), json!(format!("@{}", handle.handle)));
                entry.insert("role".into(), json!(row.description));
                if handle.handle.as_str() == crate::LEAD_HANDLE {
                    entry.insert("lead".into(), json!(true));
                }
                if row.id == ctx.agent_id {
                    // Nothing else tells an agent its own handle, and
                    // `IssueUpdate` takes handles — so without this the
                    // lead cannot reliably assign work to itself.
                    entry.insert("you".into(), json!(true));
                }
                // Load comes from runs, never from the In Progress column:
                // a run outlives the column, and a held run is not work.
                let working: Vec<i64> = load
                    .working
                    .iter()
                    .filter(|run| run.agent_id == row.id)
                    .map(|run| run.number)
                    .collect();
                if !working.is_empty() {
                    entry.insert("working_on".into(), json!(working));
                }
                Some(Value::Object(entry))
            })
            .collect();

        let mut board_facts = serde_json::Map::new();
        let held: Vec<i64> = load.held.iter().map(|run| run.number).collect();
        if !held.is_empty() {
            board_facts.insert("held".into(), json!(held));
        }
        if let Some((spent, limit)) = load.headroom.figures() {
            board_facts.insert(
                "budget".into(),
                json!({
                    "spent": usd(spent),
                    "limit": usd(limit),
                    "exhausted": load.headroom.is_exhausted(),
                }),
            );
        }

        // One pass over the board rather than a rescan per card.
        let mut children: std::collections::HashMap<&baybo_model::IssueId, Vec<&IssueRow>> =
            std::collections::HashMap::new();
        for issue in &board {
            if let Some(parent) = issue.parent_issue_id.as_ref() {
                children.entry(parent).or_default().push(issue);
            }
        }
        let rows: Vec<Value> = issues
            .iter()
            .map(|issue| {
                let mut row = render_issue(issue, &team);
                let Value::Object(map) = &mut row else {
                    return row;
                };
                if let Some(parent) = issue
                    .parent_issue_id
                    .as_ref()
                    .and_then(|id| board.iter().find(|candidate| &candidate.id == id))
                {
                    map.insert("parent".into(), json!(parent.number));
                    map.insert("stage".into(), json!(issue.stage));
                }
                if let Some(kids) = children.get(&issue.id) {
                    let owned: Vec<IssueRow> = kids.iter().map(|k| (*k).clone()).collect();
                    let (done, total) = crate::progress(&owned);
                    map.insert(
                        "sub_issues".into(),
                        json!({
                            "done": done,
                            "total": total,
                            "open_stages": crate::open_stages(&owned),
                        }),
                    );
                }
                row
            })
            .collect();

        Ok(ToolOutput::Json(json!({
            "count": rows.len(),
            "issues": rows,
            "team": roster,
            "board": Value::Object(board_facts),
        })))
    }
}
