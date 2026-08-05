//! The board's tools: what an agent working a project can do to it.
//!
//! Every one of these is scoped to the project the calling session belongs
//! to, read off [`baybo_tools::ToolContext::session_trigger`] rather than
//! taken as a parameter. That is the whole security model here — a tool
//! that accepted a `project_id` would let one board's agent edit another's,
//! and no amount of validation downstream can recover a scope the caller
//! was allowed to choose.
//!
//! Agents address issues by `#number` and each other by `@handle`, never by
//! ULID. Those are the identifiers a person reads on the board, which means
//! an agent's comment and the operator's comment refer to the same things
//! by the same names.

mod agent_create;
mod issue_comment;
mod issue_create;
mod issue_get;
mod issue_list;
mod issue_update;

use std::sync::Arc;

use baybo_model::{AgentProfileId, ProjectId};
use baybo_store::project::{IssueActor, IssuePriority, IssueRow, IssueStatus};
use baybo_tools::{Tool, ToolContext, ToolError, ToolManifest};
use serde_json::{Value, json};

use crate::ProjectManager;

pub use agent_create::PROJECT_AGENT_CREATE_TOOL_NAME;
pub use issue_comment::ISSUE_COMMENT_TOOL_NAME;
pub use issue_create::ISSUE_CREATE_TOOL_NAME;
pub use issue_get::ISSUE_GET_TOOL_NAME;
pub use issue_list::ISSUE_LIST_TOOL_NAME;
pub use issue_update::ISSUE_UPDATE_TOOL_NAME;

/// Every board tool with its manifest, ready to register.
///
/// `Trusted` with no capabilities: they write rows in baybo's own store, not
/// the host filesystem or network, so `accessed_resources` stays empty and
/// the approval gate is a no-op. What bounds them is the project scope, and
/// that comes from the session rather than from a parameter an approval
/// prompt could not check anyway.
pub fn agent_tools(manager: Arc<ProjectManager>) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(issue_list::IssueListTool::new(Arc::clone(&manager))),
        Arc::new(issue_get::IssueGetTool::new(Arc::clone(&manager))),
        Arc::new(issue_create::IssueCreateTool::new(Arc::clone(&manager))),
        Arc::new(issue_update::IssueUpdateTool::new(Arc::clone(&manager))),
        Arc::new(issue_comment::IssueCommentTool::new(Arc::clone(&manager))),
        Arc::new(agent_create::ProjectAgentCreateTool::new(manager)),
    ];
    tools
        .into_iter()
        .map(|tool| {
            let manifest = ToolManifest {
                name: tool.name().to_string(),
                description: tool.description(),
                trust_level: baybo_model::TrustLevel::Trusted,
                parameters_schema: tool.parameters_schema(),
                capabilities: vec![],
                channels: Vec::new(),
            };
            (tool, manifest)
        })
        .collect()
}

/// Which board this call is allowed to touch.
///
/// Fails closed. A session with no project has no board these tools could
/// mean, and the registry already keeps them out of such a session's tool
/// list — so reaching this branch means the scope check is the only thing
/// standing between a stray call and somebody else's data.
fn scope(ctx: &ToolContext) -> Result<ProjectId, ToolError> {
    ctx.session_trigger
        .issue()
        .map(|(project_id, _, _)| project_id.clone())
        .ok_or_else(|| {
            ToolError::Execution(
                "this session does not belong to a project board, so there is no board to act on"
                    .to_owned(),
            )
        })
}

/// Who the timeline records for this call. Always the calling agent — a
/// tool cannot claim the operator said something.
fn actor(ctx: &ToolContext) -> IssueActor {
    IssueActor::Agent(ctx.agent_id.clone())
}

/// Resolve `@handle` (with or without the `@`) to the agent it names on this
/// board.
///
/// Handles, not ids: an agent reads `@dev-1` off a card and a comment, so
/// that is what it should be able to write back. Resolution is scoped to the
/// project, which is also what makes a handle from another board simply not
/// exist rather than silently work.
async fn resolve_handle(
    manager: &ProjectManager,
    project: &ProjectId,
    handle: &str,
) -> Result<AgentProfileId, ToolError> {
    let wanted = handle.trim().trim_start_matches('@');
    let team = manager
        .team(project)
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;
    team.into_iter()
        .find(|row| {
            row.team
                .as_ref()
                .is_some_and(|t| t.handle.as_str() == wanted)
        })
        .map(|row| row.id)
        .ok_or_else(|| {
            ToolError::InvalidParams(format!(
                "no agent @{wanted} on this project. Use IssueList to see who is on the team."
            ))
        })
}

/// The reverse: an id as the handle a reader would recognise.
fn handle_of(team: &[baybo_store::AgentProfileRow], id: &AgentProfileId) -> String {
    team.iter()
        .find(|row| &row.id == id)
        .and_then(|row| row.team.as_ref())
        .map(|t| format!("@{}", t.handle))
        // A removed teammate is not on the roster but is still named by the
        // work it did, so the id is the honest answer rather than a blank.
        .unwrap_or_else(|| id.as_str().to_owned())
}

fn status_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "enum": IssueStatus::ALL.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        "description": description,
    })
}

fn priority_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "enum": IssuePriority::ALL.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        "description": description,
    })
}

fn parse_status(raw: &str) -> Result<IssueStatus, ToolError> {
    IssueStatus::parse(raw)
        .ok_or_else(|| ToolError::InvalidParams(format!("unknown status {raw:?}")))
}

fn parse_priority(raw: &str) -> Result<IssuePriority, ToolError> {
    IssuePriority::parse(raw)
        .ok_or_else(|| ToolError::InvalidParams(format!("unknown priority {raw:?}")))
}

/// The card as the model reads it in a list: enough to decide what to do
/// next, and nothing that would cost a paragraph per row.
fn render_issue(issue: &IssueRow, team: &[baybo_store::AgentProfileRow]) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("number".into(), json!(issue.number));
    obj.insert("title".into(), json!(issue.title));
    obj.insert("status".into(), json!(issue.status.as_str()));
    if issue.priority != IssuePriority::None {
        obj.insert("priority".into(), json!(issue.priority.as_str()));
    }
    if let Some(assignee) = issue.assignee.as_ref() {
        obj.insert("assignee".into(), json!(handle_of(team, assignee)));
    }
    if let Some(reason) = issue.blocked_reason.as_ref() {
        obj.insert("blocked".into(), json!(reason));
    }
    if issue.cancelled_at.is_some() {
        obj.insert("cancelled".into(), json!(true));
    }
    if let Some(branch) = issue.branch.as_ref() {
        obj.insert("branch".into(), json!(branch));
    }
    Value::Object(obj)
}

/// Micro-USD as the dollars a reader thinks in.
///
/// Presentation only — the `i64` stays the value everywhere else, because
/// money never rides a float here. One helper rather than a `format!` per
/// site, so two readers cannot round differently.
pub(super) fn usd(micros: i64) -> String {
    format!(
        "${:.2}",
        micros as f64 / baybo_model::MicroUsd::PER_USD as f64
    )
}

fn exec_err(e: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(e.to_string())
}

/// Map a domain refusal onto the tool error the model should see.
///
/// An invalid write is the model's mistake and it can fix it, so it becomes
/// `InvalidParams` with the reason intact. Everything else is the system's.
fn project_err(e: crate::ProjectError) -> ToolError {
    match e {
        crate::ProjectError::Invalid { .. }
        | crate::ProjectError::NoSuchIssue { .. }
        | crate::ProjectError::Archived(_) => ToolError::InvalidParams(e.to_string()),
        other => ToolError::Execution(other.to_string()),
    }
}
