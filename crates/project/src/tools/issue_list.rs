use std::sync::Arc;

use async_trait::async_trait;
use baybo_store::project::{IssuePriority, IssueRow, IssueStatus};
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    NOBODY_WORD, exec_err, parse_status, render_issue, scope, status_filter_schema, tokens, usd,
};
use crate::ProjectManager;

pub const ISSUE_LIST_TOOL_NAME: &str = "IssueList";

/// How much of the Done column one read returns.
///
/// The live columns are a working set someone keeps small, so they come
/// back whole. Done is the one column that only grows — nothing purges an
/// issue row — and on a real board it is already most of the response, so
/// it is the only one that needs a ceiling.
const MAX_DONE_CARDS: usize = 15;

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
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    before: Option<i64>,
}

/// Case-insensitive substring over the card's own prose.
///
/// The description is matched but never returned: it is the biggest field
/// on the row and the reason this tool omits it, so searching it costs the
/// caller nothing, while leaving out the one place a card is described in
/// its own words would make the search miss most of what it is asked for.
fn matches(issue: &IssueRow, needle: &str) -> bool {
    issue.title.to_lowercase().contains(needle) || issue.description.to_lowercase().contains(needle)
}

#[async_trait]
impl Tool for IssueListTool {
    fn name(&self) -> &str {
        ISSUE_LIST_TOOL_NAME
    }

    fn description(&self) -> String {
        format!(
            r#"List the issues on this project's board. Returns each card's number, title, status, priority, assignee handle, and branch if it has produced one. Filter with `status` (one column) and `assignee` (an `@handle`, or `{NOBODY_WORD}` for the cards nobody has picked up — that set is what triage is about). Cancelled issues are left out unless you ask for them.

Rows come back **most urgent first within each column**, so the order is already a triage order.

Every live column comes back whole. **Done does not**: it is the one column that only grows, so a read returns its {MAX_DONE_CARDS} highest-numbered cards and a `done_omitted` count for the rest. That is card order, not finish order — the board records when a card was opened, not when it landed. To read further back, pass the response's `done_continue_before` as `before`. To find one card you remember but cannot number, use `query` rather than walking back a page at a time.

Alongside them: `team`, where each member's `working_on` is what they have in flight **right now** — which is not the same as which column a card sits in, because a run outlives the column it started in. `you` marks your own entry, so you know the handle to assign work to yourself. And `board`, which says what is held and what is left of today's budget: promoting a card on an exhausted board records a run that does not start."#
        )
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": status_filter_schema("Only issues in this column. Use `all` when the strict schema requires a value but no status filter is wanted."),
                "assignee": super::assignee_schema(false),
                "include_cancelled": {
                    "type": "boolean",
                    "description": "Include cancelled issues. Default false — they are not live work. Not consulted while `query` is set: a search already spans them.",
                },
                "query": {
                    "type": "string",
                    "description": "Keep only cards whose title or description contains this text, case-insensitively. A search spans the whole board — every column, and cancelled cards too, which come back marked — because a search that hides a match reads as `no such card`. This is the door for `there was a card about X`; use `IssueGet` once you have its number.",
                },
                "before": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Keep only cards numbered below this. Pass the response's `done_continue_before` to read the next page further back in Done. Use `0` when the strict schema requires a value but no bound is wanted.",
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
        let status = p
            .status
            .as_deref()
            .map(str::trim)
            .filter(|status| !status.is_empty() && *status != super::ALL_STATUSES_FILTER)
            .map(parse_status)
            .transpose()?;
        // An empty query is no question, so it excludes nothing — the same
        // reading `parse_assignee_filter` gives an empty filter.
        let query = p
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        // Card numbers are 1-based, so a bound at or below the first card
        // would answer "no such card" for a whole board. `0` is what a
        // model reaches for when an integer parameter has no "unset"
        // spelling, and it read as the empty board on every call that sent
        // it — the same reading an empty `query` or `assignee` is already
        // given here.
        let before = p.before.filter(|before| *before > 0);

        let team = self.manager.team(&project).await.map_err(exec_err)?;
        let load = self.manager.board_load(&project).await.map_err(exec_err)?;
        let board = self.manager.list_issues(&project).await.map_err(exec_err)?;
        let wanted_assignee =
            super::parse_assignee_filter(&self.manager, &project, p.assignee.as_deref()).await?;

        let mut issues = board.clone();
        issues.retain(|issue| {
            // A search spans cancelled cards whatever `include_cancelled`
            // says. Leaving out a match would answer "no such card", and a
            // wrong answer to a question about existence is worse than a
            // longer list — the row comes back marked either way.
            (query.is_some() || p.include_cancelled || issue.cancelled_at.is_none())
                && status.is_none_or(|s| issue.status == s)
                && wanted_assignee
                    .as_ref()
                    .is_none_or(|wanted| &issue.assignee == wanted)
                && before.is_none_or(|before| issue.number < before)
                && query.as_deref().is_none_or(|needle| matches(issue, needle))
        });

        // `number` is the only key a page can be cut on: it is dense,
        // unique per board and never rewritten, where `position` is
        // whatever order the operator last dragged the column into and no
        // column records when a card was finished.
        let (mut issues, mut done): (Vec<IssueRow>, Vec<IssueRow>) = issues
            .into_iter()
            .partition(|issue| issue.status != IssueStatus::Done);
        done.sort_by_key(|issue| std::cmp::Reverse(issue.number));
        let done_omitted = done.len().saturating_sub(MAX_DONE_CARDS);
        done.truncate(MAX_DONE_CARDS);
        let done_continue_before = done
            .last()
            .map(|issue| issue.number)
            .filter(|_| done_omitted > 0);
        issues.append(&mut done);

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
        if let Some(figures) = load.headroom.figures() {
            // Render the figures in the constraining ceiling's unit.
            let (spent, limit) = match figures {
                crate::budget::Figures::Money {
                    spent_micros,
                    limit_micros,
                } => (usd(spent_micros), usd(limit_micros)),
                crate::budget::Figures::Tokens {
                    spent_tokens,
                    limit_tokens,
                } => (tokens(spent_tokens), tokens(limit_tokens)),
            };
            board_facts.insert(
                "budget".into(),
                json!({
                    "spent": spent,
                    "limit": limit,
                    "exhausted": load.headroom.is_exhausted(),
                }),
            );
        }

        // One pass over the board rather than a rescan per card.
        let mut children: std::collections::HashMap<&baybo_model::IssueId, Vec<&IssueRow>> =
            std::collections::HashMap::new();
        let mut by_id: std::collections::HashMap<&baybo_model::IssueId, &IssueRow> =
            std::collections::HashMap::new();
        for issue in &board {
            by_id.insert(&issue.id, issue);
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
                    .and_then(|id| by_id.get(id).copied())
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
                            "open_stages": crate::stages::open_stages(&owned),
                        }),
                    );
                }
                row
            })
            .collect();

        let mut out = serde_json::Map::new();
        out.insert("count".into(), json!(rows.len()));
        out.insert("issues".into(), json!(rows));
        if done_omitted > 0 {
            out.insert("done_omitted".into(), json!(done_omitted));
        }
        if let Some(before) = done_continue_before {
            out.insert("done_continue_before".into(), json!(before));
        }
        out.insert("team".into(), json!(roster));
        out.insert("board".into(), Value::Object(board_facts));
        Ok(ToolOutput::Json(Value::Object(out)))
    }
}
