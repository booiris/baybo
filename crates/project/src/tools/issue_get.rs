use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::AgentProfileId;
use baybo_store::project::{IssueEventBody, IssueEventRow};
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{exec_err, handle_of, project_err, render_issue, scope, usd};
use crate::ProjectManager;

pub const ISSUE_GET_TOOL_NAME: &str = "IssueGet";

/// How many timeline entries to return, newest last.
///
/// A cap rather than the whole history: a long-running card accumulates a
/// run-started/run-settled pair per attempt, and a tool result that grows
/// without bound is a tool result that eventually costs more than the work.
const MAX_TIMELINE_ENTRIES: usize = 40;

pub(super) struct IssueGetTool {
    manager: Arc<ProjectManager>,
}

impl IssueGetTool {
    pub(super) fn new(manager: Arc<ProjectManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    number: i64,
}

#[async_trait]
impl Tool for IssueGetTool {
    fn name(&self) -> &str {
        ISSUE_GET_TOOL_NAME
    }

    fn description(&self) -> String {
        r#"Read one issue on this project's board in full: its description, properties, and its timeline — the comments and system events, in order, that say what has happened to it. Use it before acting on a card somebody else has been working, so you answer what was actually said rather than what the title suggests."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "number": { "type": "integer", "description": "The issue's number on this board (the `#N` on its card)." },
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

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::ProjectBoard
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let project = scope(ctx)?;
        let issue = self
            .manager
            .get_issue(&project, p.number)
            .await
            .map_err(project_err)?;
        let team = self.manager.team(&project).await.map_err(exec_err)?;
        let events = self
            .manager
            .timeline(&project, p.number)
            .await
            .map_err(project_err)?;

        let skipped = events.len().saturating_sub(MAX_TIMELINE_ENTRIES);
        let shown = &events[skipped..];
        // A timeline is permanent history, so it names agents the live
        // roster no longer carries — and the roster is what `team` is.
        // Nothing unassigns a departed teammate's cards either, so the
        // assignee is asked for too. Whoever the card names but the roster
        // does not is resolved one by one, so a teammate that has left
        // still reads as `@handle` instead of a ULID nobody recognises.
        let mut known = team;
        let mut absent: Vec<AgentProfileId> = Vec::new();
        for id in issue
            .assignee
            .iter()
            .cloned()
            .chain(shown.iter().flat_map(named_agents))
        {
            if !known.iter().any(|row| row.id == id) && !absent.contains(&id) {
                absent.push(id);
            }
        }
        known.extend(self.manager.agent_profiles(absent).await);

        let timeline: Vec<Value> = shown
            .iter()
            .map(|entry| {
                let who = match &entry.actor {
                    baybo_store::project::IssueActor::User => "the operator".to_owned(),
                    baybo_store::project::IssueActor::System => "the board".to_owned(),
                    baybo_store::project::IssueActor::Agent(id) => handle_of(&known, id),
                };
                json!({ "at": entry.created_at.to_rfc3339(), "by": who, "event": narrate(&entry.body, &known) })
            })
            .collect();

        let mut out = render_issue(&issue, &known);
        if let Value::Object(map) = &mut out {
            map.insert("description".into(), json!(issue.description));
            map.insert("timeline".into(), json!(timeline));
            if skipped > 0 {
                map.insert("timeline_older_entries_omitted".into(), json!(skipped));
            }
        }
        Ok(ToolOutput::Json(out))
    }
}

/// Every agent one timeline entry names — its actor, plus both sides of a
/// reassignment, which names two more agents than its actor does and
/// renders both.
fn named_agents(entry: &IssueEventRow) -> Vec<AgentProfileId> {
    let mut ids = Vec::new();
    if let baybo_store::project::IssueActor::Agent(id) = &entry.actor {
        ids.push(id.clone());
    }
    if let IssueEventBody::Assigned { from, to } = &entry.body {
        ids.extend(from.iter().chain(to.iter()).cloned());
    }
    ids
}

/// One timeline entry as a sentence.
///
/// Prose rather than the tagged JSON the web client renders: the reader
/// here is a model assembling context, and "moved from todo to in_progress"
/// costs fewer tokens than the object it came from and needs no schema to
/// interpret.
///
/// `known` carries the agents the entry may name. Agents address each other
/// by handle everywhere else on the board, so a sentence that reads
/// "assigned it to 01JC3KQ4…" is one the next run cannot act on.
fn narrate(body: &IssueEventBody, known: &[baybo_store::AgentProfileRow]) -> String {
    match body {
        IssueEventBody::Comment { text } => text.clone(),
        IssueEventBody::Opened => "opened the issue".to_owned(),
        IssueEventBody::Moved { from, to } => {
            format!("moved it from {} to {}", from.as_str(), to.as_str())
        }
        IssueEventBody::Assigned { from, to } => match (from, to) {
            (_, Some(to)) => format!("assigned it to {}", handle_of(known, to)),
            (Some(from), None) => format!("unassigned {}", handle_of(known, from)),
            (None, None) => "left it unassigned".to_owned(),
        },
        IssueEventBody::RunStarted {
            attempt, trigger, ..
        } => {
            format!("started run #{attempt} ({})", trigger.as_str())
        }
        IssueEventBody::RunSettled {
            attempt,
            status,
            error,
            ..
        } => match error {
            Some(error) => format!("run #{attempt} {}: {error}", status.as_str()),
            None => format!("run #{attempt} {}", status.as_str()),
        },
        IssueEventBody::Blocked { reason } => format!("blocked it: {reason}"),
        IssueEventBody::Unblocked => "unblocked it".to_owned(),
        IssueEventBody::Cancelled => "cancelled it".to_owned(),
        IssueEventBody::WorktreeReclaimed { branch_deleted } => {
            if *branch_deleted {
                // Not "nothing was committed": a branch merged before the
                // card was dragged to Done also counts zero commits ahead,
                // and git agrees to drop it.
                "reclaimed the worktree; the branch held nothing the repository did not already \
                 have, so it went too"
                    .to_owned()
            } else {
                "reclaimed the worktree; the branch was kept".to_owned()
            }
        }
        IssueEventBody::WorktreeKept { reason } => {
            format!("left the worktree in place: {reason}")
        }
        IssueEventBody::ApprovalRequested { tool, summary, .. } => {
            format!("asked the operator to approve a {tool} call: {summary}")
        }
        IssueEventBody::ApprovalResolved { decision, .. } => {
            format!("the operator answered: {}", decision.as_str())
        }
        IssueEventBody::StageCompleted { stage } => {
            format!("stage {stage} finished — every step in it is done")
        }
        IssueEventBody::BudgetExhausted {
            spent_micros,
            limit_micros,
        } => format!(
            "held the run: the project has spent {} of its {} daily budget",
            usd(*spent_micros),
            usd(*limit_micros)
        ),
        IssueEventBody::BudgetRestored {
            spent_micros,
            limit_micros,
        } => format!(
            "started the held run: {} of {} spent today",
            usd(*spent_micros),
            usd(*limit_micros)
        ),
    }
}
