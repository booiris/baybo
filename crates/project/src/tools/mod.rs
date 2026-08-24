//! The board's tools: what an agent working a project can do to it.

mod agent_create;
mod issue_comment;
mod issue_create;
mod issue_get;
mod issue_list;
mod issue_merge;
mod issue_update;

use std::sync::Arc;

use baybo_model::{AgentProfileId, IssueId, ProjectId};
use baybo_store::project::{IssueActor, IssuePriority, IssueRow, IssueStatus};
use baybo_tools::{Tool, ToolContext, ToolError, ToolManifest};
use serde_json::{Value, json};

use crate::ProjectManager;
use crate::actors::handle_of;

pub use agent_create::PROJECT_AGENT_CREATE_TOOL_NAME;
pub use issue_comment::ISSUE_COMMENT_TOOL_NAME;
pub use issue_create::ISSUE_CREATE_TOOL_NAME;
pub use issue_get::ISSUE_GET_TOOL_NAME;
pub use issue_list::ISSUE_LIST_TOOL_NAME;
pub use issue_merge::ISSUE_MERGE_TOOL_NAME;
pub use issue_update::ISSUE_UPDATE_TOOL_NAME;

/// Every board tool with its manifest, ready to register.
pub fn agent_tools(manager: Arc<ProjectManager>) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(issue_list::IssueListTool::new(Arc::clone(&manager))),
        Arc::new(issue_get::IssueGetTool::new(Arc::clone(&manager))),
        Arc::new(issue_create::IssueCreateTool::new(Arc::clone(&manager))),
        Arc::new(issue_update::IssueUpdateTool::new(Arc::clone(&manager))),
        Arc::new(issue_comment::IssueCommentTool::new(Arc::clone(&manager))),
        Arc::new(issue_merge::IssueMergeTool::new(Arc::clone(&manager))),
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

fn scope(ctx: &ToolContext) -> Result<ProjectId, ToolError> {
    ctx.session_trigger.project().cloned().ok_or_else(|| {
        ToolError::Execution(
            "this session does not belong to a project board, so there is no board to act on"
                .to_owned(),
        )
    })
}

fn actor(ctx: &ToolContext) -> IssueActor {
    IssueActor::Agent(ctx.agent_id.clone())
}

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

const CRON_SOURCE_KEY_PREFIX: &str = "cron:";

fn source_key(ctx: &ToolContext, suffix: Option<&str>) -> Option<String> {
    let job = ctx.session_trigger.cron_job_id()?;
    ctx.session_trigger.project()?;
    Some(match suffix.map(str::trim).filter(|s| !s.is_empty()) {
        Some(suffix) => format!("{CRON_SOURCE_KEY_PREFIX}{job}:{suffix}"),
        None => format!("{CRON_SOURCE_KEY_PREFIX}{job}"),
    })
}

/// The card whose run is making this call, when the call comes from one.
///
/// Derived, never a parameter: the session already carries the answer with
/// certainty, and a model asked to restate it can forget it, mistype it, or
/// confuse it with `parent`. `source_key` above is the same trade — the
/// server owns the identity and the model supplies at most a suffix.
///
/// `None` from a cron fire and from the operator's own create door. Those
/// cards are roots, which is the truth rather than a gap.
fn filed_from(ctx: &ToolContext) -> Option<IssueId> {
    ctx.session_trigger
        .issue()
        .map(|(_, issue_id, _)| issue_id.clone())
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

/// The words that mean "nobody" wherever a tool takes an assignee. All three
/// are accepted everywhere, because a model that learned one on `IssueUpdate`
/// should not be refused for using it on `IssueList`.
const NOBODY: &[&str] = &["none", "unassigned", "nobody"];

fn is_nobody(raw: &str) -> bool {
    NOBODY.iter().any(|w| raw.eq_ignore_ascii_case(w))
}

/// The word the schemas quote when they mean "nobody". One spelling in the
/// prose, all of [`NOBODY`] accepted at the door.
const NOBODY_WORD: &str = NOBODY[0];

/// What the `assignee` parameter says, in the two shapes it has. Written
/// once because a model that reads one of these tools is about to use the
/// others, and three spellings of one rule is how it learns a wrong one.
fn assignee_schema(setting: bool) -> Value {
    let description = if setting {
        format!("An `@handle` from this project's team, or `{NOBODY_WORD}` for nobody.")
    } else {
        format!(
            "Keep only issues assigned to this `@handle`, or `{NOBODY_WORD}` for the ones \
             nobody is on. Omit it (or pass an empty string) to filter by something else."
        )
    };
    json!({ "type": "string", "description": description })
}

/// An assignee a tool is about to **set**: `Some(None)` unassigns.
/// An empty string counts as "nobody" here — the field carries a value, and
/// an empty one is the absence of an assignee.
async fn parse_assignee_value(
    manager: &ProjectManager,
    project: &ProjectId,
    raw: Option<&str>,
) -> Result<Option<Option<AgentProfileId>>, ToolError> {
    match raw.map(str::trim) {
        None => Ok(None),
        Some(raw) if raw.is_empty() || is_nobody(raw) => Ok(Some(None)),
        Some(raw) => Ok(Some(Some(resolve_handle(manager, project, raw).await?))),
    }
}

/// An assignee a tool is about to **filter** by: `Some(None)` keeps only
/// unassigned cards.
///
/// An empty string is "no filter" here, the opposite of what it means when
/// setting — a filter is a question, and an empty question excludes nothing.
/// Every logged `assignee: ""` came paired with a status filter, i.e. "list
/// in-progress cards, whoever holds them", and answering it with the
/// unassigned ones would be a wrong list rather than a loud error.
async fn parse_assignee_filter(
    manager: &ProjectManager,
    project: &ProjectId,
    raw: Option<&str>,
) -> Result<Option<Option<AgentProfileId>>, ToolError> {
    match raw.map(str::trim) {
        None => Ok(None),
        Some("") => Ok(None),
        Some(raw) if is_nobody(raw) => Ok(Some(None)),
        Some(raw) => Ok(Some(Some(resolve_handle(manager, project, raw).await?))),
    }
}

fn parse_status(raw: &str) -> Result<IssueStatus, ToolError> {
    IssueStatus::parse(raw)
        .ok_or_else(|| ToolError::InvalidParams(format!("unknown status {raw:?}")))
}

fn parse_priority(raw: &str) -> Result<IssuePriority, ToolError> {
    IssuePriority::parse(raw)
        .ok_or_else(|| ToolError::InvalidParams(format!("unknown priority {raw:?}")))
}

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
    // The card's own files, named the same way the run brief names them. An
    // operator's attachment is otherwise invisible to a lookup: the brief
    // mentions it once, and `IssueGet` — where an agent goes when the brief
    // has scrolled past — would not.
    if !issue.attachments.is_empty() {
        obj.insert(
            "attachments".into(),
            json!(
                issue
                    .attachments
                    .iter()
                    .map(crate::attachments::describe)
                    .collect::<Vec<_>>()
            ),
        );
    }
    Value::Object(obj)
}

/// Micro-USD as the dollars a reader thinks in.
pub(super) fn usd(micros: i64) -> String {
    format!(
        "${:.2}",
        micros as f64 / baybo_model::MicroUsd::PER_USD as f64
    )
}

/// An exact token count with its unit.
pub(super) fn tokens(count: i64) -> String {
    format!("{count} tokens")
}

fn exec_err(e: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(e.to_string())
}

fn project_err(e: crate::ProjectError) -> ToolError {
    match e {
        crate::ProjectError::Invalid { .. }
        | crate::ProjectError::NoSuchIssue { .. }
        | crate::ProjectError::Archived(_) => ToolError::InvalidParams(e.to_string()),
        other => ToolError::Execution(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool takes every word for "nobody"; a model that learned one on
    /// `IssueUpdate` is not refused for using it on `IssueList`.
    #[test]
    fn the_words_for_nobody_are_the_same_everywhere() {
        for word in ["none", "NONE", "unassigned", "Nobody"] {
            assert!(is_nobody(word), "{word}");
        }
        for word in ["nobody-else", "@none", "n"] {
            assert!(!is_nobody(word), "{word}");
        }
    }

    /// The one place the two shapes deliberately disagree, pinned so a later
    /// reader cannot quietly invert it: an empty string sets nobody, and
    /// filters nobody out of the question.
    #[test]
    fn an_empty_assignee_unassigns_when_set_and_filters_nothing_when_asked() {
        assert!(
            assignee_schema(true)["description"]
                .as_str()
                .is_some_and(|d| d.contains("nobody")),
            "the setting shape must name the word that unassigns"
        );
        assert!(
            assignee_schema(false)["description"]
                .as_str()
                .is_some_and(|d| d.contains("empty string")),
            "the filter shape must say what an empty one does"
        );
    }
}
