use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::AgentProfileId;
use baybo_store::project::{IssueEventBody, IssueEventRow};
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{exec_err, project_err, render_issue, scope, tokens, usd};
use crate::ProjectManager;
use crate::actors::{OPERATOR, handle_of, label, named_agent};

pub const ISSUE_GET_TOOL_NAME: &str = "IssueGet";

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
        known.extend(self.manager.agent_profiles(&project, absent).await);

        let timeline: Vec<Value> = shown
            .iter()
            .map(|entry| {
                let who = label(&entry.actor, &known);
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

fn named_agents(entry: &IssueEventRow) -> Vec<AgentProfileId> {
    let mut ids = Vec::new();
    ids.extend(named_agent(&entry.actor));
    if let IssueEventBody::Assigned { from, to } = &entry.body {
        ids.extend(from.iter().chain(to.iter()).cloned());
    }
    ids
}

fn narrate(body: &IssueEventBody, known: &[baybo_store::AgentProfileRow]) -> String {
    match body {
        // The files are named, not dropped: a comment whose whole point was
        // the screenshot under it would otherwise read as an empty line, and
        // an operator's file older than this tool's timeline window would be
        // invisible to the agent that went looking for it.
        IssueEventBody::Comment { text, attachments } => match attachments.as_slice() {
            [] => text.clone(),
            files => {
                let named = files
                    .iter()
                    .map(crate::attachments::describe)
                    .collect::<Vec<_>>()
                    .join(", ");
                if text.is_empty() {
                    format!("attached {named}")
                } else {
                    format!("{text}\n(attached: {named})")
                }
            }
        },
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
        IssueEventBody::RunInterrupted {
            attempt, resumes, ..
        } => {
            format!(
                "run #{attempt} was interrupted before it finished (interruption {resumes}); the \
                 board picked it up again"
            )
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
            format!("asked {OPERATOR} to approve a {tool} call: {summary}")
        }
        IssueEventBody::ApprovalResolved {
            decision,
            resolution,
            ..
        } => match resolution {
            baybo_model::ApprovalResolution::Answered => {
                format!("{OPERATOR} answered: {}", decision.as_str())
            }
            baybo_model::ApprovalResolution::TimedOut => {
                "nobody answered within the approval window; the call was denied by default"
                    .to_owned()
            }
            baybo_model::ApprovalResolution::Abandoned => {
                "the prompt went away undecided — its run was interrupted before anyone answered"
                    .to_owned()
            }
            baybo_model::ApprovalResolution::Policy => format!(
                "resolved by standing policy without a prompt: {}",
                decision.as_str()
            ),
        },
        IssueEventBody::StageCompleted { stage } => {
            // "or called off", because a cancelled step counts out of its
            // stage — an agent told the stage is "done" would go looking for
            // work that was deliberately dropped.
            format!("stage {stage} finished — every step in it is done or called off")
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
        IssueEventBody::TokenBudgetExhausted {
            spent_tokens,
            limit_tokens,
        } => format!(
            "held the run: the project has spent {} of its {} daily token budget",
            tokens(*spent_tokens),
            tokens(*limit_tokens)
        ),
        IssueEventBody::TokenBudgetRestored {
            spent_tokens,
            limit_tokens,
        } => format!(
            "started the held run: {} of {} spent today",
            tokens(*spent_tokens),
            tokens(*limit_tokens)
        ),
    }
}
