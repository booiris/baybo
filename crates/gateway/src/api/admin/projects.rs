//! `/v1/projects/*` — kanban projects and the issues on their boards
//! (docs/todo/kanban.md).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use baybo_model::{AgentProfileId, ProjectId};
use baybo_project::{NewIssueRequest, NewProject, ProjectError};
use baybo_store::project::{
    DEFAULT_MAX_PARALLEL_ISSUE_RUNS, IssueActor, IssueEventBody, IssueEventRow, IssuePriority,
    IssueRow, IssueRunRow, IssueStatus, IssueUpdate, ProjectRow, ProjectUpdate, RunStatus,
    RunTrigger,
};

use crate::api::dto::{ErrorBody, ListResponse};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_projects, create_project))
        .routes(routes!(get_project, update_project))
        .routes(routes!(set_project_archived))
        .routes(routes!(list_issues, create_issue))
        .routes(routes!(get_issue, update_issue))
        .routes(routes!(move_issue))
        .routes(routes!(list_issue_runs))
        .routes(routes!(list_issue_events))
        .routes(routes!(project_feed))
        .routes(routes!(projects_attention))
        .routes(routes!(mark_project_read))
        .routes(routes!(resolve_approval))
        .routes(routes!(create_comment))
        .routes(routes!(list_active_runs))
        .routes(routes!(cancel_run))
        .routes(routes!(retry_run))
}

/// Map the domain's error onto a status once, for every handler here.
pub(super) fn project_err(e: ProjectError) -> GatewayError {
    match e {
        ProjectError::NoSuchProject(id) => GatewayError::NotFound(format!("project {id}")),
        ProjectError::NoSuchIssue { project, number } => {
            GatewayError::NotFound(format!("project {project} issue #{number}"))
        }
        ProjectError::Invalid { .. } => GatewayError::BadRequest(e.to_string()),
        ProjectError::Conflict(reason) => GatewayError::Conflict(reason),
        // Archived is a state the caller can fix, not a server failure: the
        // board is still there, it just isn't taking writes.
        ProjectError::Archived(_) => GatewayError::Conflict(e.to_string()),
        ProjectError::Storage(e) => GatewayError::Internal(format!("project storage: {e}")),
        ProjectError::Workdir(e) => GatewayError::Internal(format!("project workdir: {e}")),
    }
}

async fn on_board(state: &AdminState, project: &ProjectId, row: IssueRow) -> Result<IssueDto> {
    let board = state
        .project_manager
        .list_issues(project)
        .await
        .map_err(project_err)?;
    Ok(IssueDto::on_board(row, &board))
}

type ActorHandles = std::collections::HashMap<AgentProfileId, baybo_model::AgentHandle>;

async fn actor_handles(
    state: &AdminState,
    project: &ProjectId,
    rows: &[IssueEventRow],
) -> ActorHandles {
    let mut ids: std::collections::HashSet<AgentProfileId> = std::collections::HashSet::new();
    for row in rows {
        if let IssueActor::Agent(id) = &row.actor {
            ids.insert(id.clone());
        }
        // A reassignment names two more agents than its actor does, and
        // both sides are rendered.
        if let IssueEventBody::Assigned { from, to } = &row.body {
            ids.extend(from.iter().chain(to.iter()).cloned());
        }
    }
    futures::future::join_all(ids.into_iter().map(|id| async {
        let team = state
            .agent_profile_store
            .get(&id)
            .await
            .ok()
            .flatten()?
            .team?;
        (team.project_id == *project).then_some((id, team.handle))
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

/// Parse a path segment into a [`ProjectId`], running the same grammar the
/// filesystem depends on rather than trusting the URL.
pub(super) fn parse_project_id(raw: &str) -> Result<ProjectId> {
    ProjectId::parse(raw).map_err(|e| GatewayError::BadRequest(e.to_string()))
}

fn parse_assignee(raw: Option<String>) -> Result<Option<AgentProfileId>> {
    raw.map(AgentProfileId::parse)
        .transpose()
        .map_err(|e| GatewayError::BadRequest(e.to_string()))
}

fn double_option<'de, T, D>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::deserialize(deserializer).map(Some)
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectDto {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Absolute path to the git repository this project's agents work in.
    pub workdir: String,
    /// Daily spend ceiling in micro-USD. Absent means no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_budget_micros: Option<i64>,
    /// How many runs this board starts on its own, by taking cards off the
    /// top of Todo as room appears. `0` means it starts only what somebody
    /// drags into In Progress.
    pub max_parallel_issue_runs: i64,
    /// Present only while the project sits in the archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl From<ProjectRow> for ProjectDto {
    fn from(row: ProjectRow) -> Self {
        Self {
            id: row.id.to_string(),
            name: row.name,
            description: row.description,
            workdir: row.workdir,
            daily_budget_micros: row.daily_budget.map(baybo_model::MicroUsd::into_micros),
            max_parallel_issue_runs: i64::try_from(row.max_parallel_issue_runs).unwrap_or(i64::MAX),
            archived_at_ms: row.archived_at.map(|t| t.timestamp_millis()),
            created_at_ms: row.created_at.timestamp_millis(),
            updated_at_ms: row.updated_at.timestamp_millis(),
        }
    }
}

/// Which column a card sits in. Entering `in_progress` is what will start
/// an agent once execution lands, which is why the set is fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueStatusDto {
    Backlog,
    Todo,
    InProgress,
    Review,
    Done,
}

impl From<IssueStatus> for IssueStatusDto {
    fn from(status: IssueStatus) -> Self {
        match status {
            IssueStatus::Backlog => Self::Backlog,
            IssueStatus::Todo => Self::Todo,
            IssueStatus::InProgress => Self::InProgress,
            IssueStatus::Review => Self::Review,
            IssueStatus::Done => Self::Done,
        }
    }
}

impl From<IssueStatusDto> for IssueStatus {
    fn from(status: IssueStatusDto) -> Self {
        match status {
            IssueStatusDto::Backlog => Self::Backlog,
            IssueStatusDto::Todo => Self::Todo,
            IssueStatusDto::InProgress => Self::InProgress,
            IssueStatusDto::Review => Self::Review,
            IssueStatusDto::Done => Self::Done,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssuePriorityDto {
    Urgent,
    High,
    Medium,
    Low,
    #[default]
    None,
}

impl From<IssuePriority> for IssuePriorityDto {
    fn from(priority: IssuePriority) -> Self {
        match priority {
            IssuePriority::Urgent => Self::Urgent,
            IssuePriority::High => Self::High,
            IssuePriority::Medium => Self::Medium,
            IssuePriority::Low => Self::Low,
            IssuePriority::None => Self::None,
        }
    }
}

impl From<IssuePriorityDto> for IssuePriority {
    fn from(priority: IssuePriorityDto) -> Self {
        match priority {
            IssuePriorityDto::Urgent => Self::Urgent,
            IssuePriorityDto::High => Self::High,
            IssuePriorityDto::Medium => Self::Medium,
            IssuePriorityDto::Low => Self::Low,
            IssuePriorityDto::None => Self::None,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IssueDto {
    /// The human address, unique within its project: `#3`.
    pub number: i64,
    pub project_id: String,
    pub title: String,
    pub description: String,
    pub status: IssueStatusDto,
    pub priority: IssuePriorityDto,
    /// The agent on it, if any. In Progress always has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Rank within the column, dense and ascending.
    pub position: i64,
    /// The branch this issue's work landed on. Absent until it has a
    /// commit, so a research issue never shows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Why work stopped. A badge on the card — blocked work stays in
    /// whichever column it was in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// The issue this one is a step of, by its number on this board.
    /// Absent on a top-level card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<i64>,
    /// Which barrier under that parent. `0` and meaningless without one.
    pub stage: i64,
    /// This card's own steps, if it has any: how many are done out of how
    /// many still meant to happen. Cancelled steps leave both counts, so a
    /// card whose last steps were called off reads finished rather than
    /// stuck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_issues: Option<SubIssueProgress>,
    /// Present once the issue is cancelled. The row is never deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A parent card's progress ring.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct SubIssueProgress {
    pub done: i64,
    pub total: i64,
}

impl IssueDto {
    /// Build one card against the board it lives on.
    pub fn on_board(row: IssueRow, board: &[IssueRow]) -> Self {
        let parent = row.parent_issue_id.as_ref().and_then(|id| {
            board
                .iter()
                .find(|issue| &issue.id == id)
                .map(|issue| issue.number)
        });
        let children: Vec<IssueRow> = board
            .iter()
            .filter(|issue| issue.parent_issue_id.as_ref() == Some(&row.id))
            .cloned()
            .collect();
        let sub_issues = (!children.is_empty()).then(|| {
            let (done, total) = baybo_project::progress(&children);
            SubIssueProgress {
                done: done as i64,
                total: total as i64,
            }
        });
        Self {
            parent,
            sub_issues,
            ..Self::from(row)
        }
    }
}

impl From<IssueRow> for IssueDto {
    fn from(row: IssueRow) -> Self {
        Self {
            number: row.number,
            project_id: row.project_id.to_string(),
            title: row.title,
            description: row.description,
            status: row.status.into(),
            priority: row.priority.into(),
            assignee: row.assignee.map(|a| a.to_string()),
            position: row.position,
            branch: row.branch,
            blocked_reason: row.blocked_reason,
            parent: None,
            stage: row.stage,
            sub_issues: None,
            cancelled_at_ms: row.cancelled_at.map(|t| t.timestamp_millis()),
            created_at_ms: row.created_at.timestamp_millis(),
            updated_at_ms: row.updated_at.timestamp_millis(),
        }
    }
}

/// Where a run is. `queued` and `running` are the unfinished states — a
/// card showing either is a card being worked.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatusDto {
    /// Recorded but not started: the project is over its daily budget. It
    /// starts by itself once the board has headroom.
    Held,
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl From<RunStatus> for RunStatusDto {
    fn from(status: RunStatus) -> Self {
        match status {
            RunStatus::Held => Self::Held,
            RunStatus::Queued => Self::Queued,
            RunStatus::Running => Self::Running,
            RunStatus::Done => Self::Done,
            RunStatus::Failed => Self::Failed,
            RunStatus::Cancelled => Self::Cancelled,
        }
    }
}

/// A run ceiling off the wire, as the board's own type.
///
/// The board itself sets no upper bound — how much work it may start at
/// once is the operator's call. What it cannot represent is a negative, and
/// that has to be refused *here*, at the conversion: `usize::try_from` is
/// the only place the sign still exists, and letting one through as a
/// saturated `usize` would hand the driver a slot count that empties the
/// whole Todo column in one pass.
fn parallel_issue_runs(value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        GatewayError::BadRequest(
            "max_parallel_issue_runs must not be negative — use 0 to stop the board \
             starting work by itself"
                .to_owned(),
        )
    })
}

/// Why a run was started.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunTriggerDto {
    Started,
    Assigned,
    Retry,
    Comment,
    Promoted,
    Triage,
    StageBarrier,
}

impl From<RunTrigger> for RunTriggerDto {
    fn from(trigger: RunTrigger) -> Self {
        match trigger {
            RunTrigger::Started => Self::Started,
            RunTrigger::Assigned => Self::Assigned,
            RunTrigger::Retry => Self::Retry,
            RunTrigger::Comment => Self::Comment,
            RunTrigger::Promoted => Self::Promoted,
            RunTrigger::Triage => Self::Triage,
            RunTrigger::StageBarrier => Self::StageBarrier,
        }
    }
}

/// Mirror of [`baybo_model::ApprovalDecision`], so the client gets the same
/// discriminated union it switches on everywhere else here.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecisionDto {
    Approve,
    ApproveAlways,
    Deny,
}

impl From<baybo_model::ApprovalDecision> for ApprovalDecisionDto {
    fn from(decision: baybo_model::ApprovalDecision) -> Self {
        match decision {
            baybo_model::ApprovalDecision::Approve => Self::Approve,
            baybo_model::ApprovalDecision::ApproveAlways => Self::ApproveAlways,
            baybo_model::ApprovalDecision::Deny => Self::Deny,
        }
    }
}

impl From<ApprovalDecisionDto> for baybo_model::ApprovalDecision {
    fn from(decision: ApprovalDecisionDto) -> Self {
        match decision {
            ApprovalDecisionDto::Approve => Self::Approve,
            ApprovalDecisionDto::ApproveAlways => Self::ApproveAlways,
            ApprovalDecisionDto::Deny => Self::Deny,
        }
    }
}

/// An agent as a timeline entry names it.
#[derive(Debug, Serialize, ToSchema)]
pub struct AgentRefDto {
    pub id: String,
    /// The `@handle` to render, without the `@`. Falls back to the id when
    /// the reference resolves to nothing at all.
    pub handle: String,
}

/// Who did the thing an entry records.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActorDto {
    /// The operator working the board.
    User,
    /// The board acting on its own — today, the budget gate.
    System,
    Agent(AgentRefDto),
}

fn agent_ref(id: &AgentProfileId, handles: &ActorHandles) -> AgentRefDto {
    AgentRefDto {
        id: id.as_str().to_owned(),
        handle: handles
            .get(id)
            .map(|handle| handle.as_str().to_owned())
            .unwrap_or_else(|| id.as_str().to_owned()),
    }
}

/// What one timeline entry says.
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IssueEventBodyDto {
    Comment {
        text: String,
    },
    Opened,
    Moved {
        from: IssueStatusDto,
        to: IssueStatusDto,
    },
    Assigned {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<AgentRefDto>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<AgentRefDto>,
    },
    RunStarted {
        attempt: i64,
        trigger: RunTriggerDto,
    },
    RunSettled {
        attempt: i64,
        status: RunStatusDto,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Blocked {
        reason: String,
    },
    Unblocked,
    Cancelled,
    WorktreeReclaimed {
        branch_deleted: bool,
    },
    WorktreeKept {
        reason: String,
    },
    ApprovalRequested {
        call_id: String,
        tool: String,
        summary: String,
    },
    ApprovalResolved {
        call_id: String,
        decision: ApprovalDecisionDto,
    },
    StageCompleted {
        stage: i64,
    },
    BudgetExhausted {
        spent_micros: i64,
        limit_micros: i64,
    },
    BudgetRestored {
        spent_micros: i64,
        limit_micros: i64,
    },
}

impl IssueEventBodyDto {
    fn with_handles(body: IssueEventBody, handles: &ActorHandles) -> Self {
        match body {
            IssueEventBody::ApprovalRequested {
                call_id,
                tool,
                summary,
            } => Self::ApprovalRequested {
                call_id,
                tool,
                summary,
            },
            IssueEventBody::ApprovalResolved { call_id, decision } => Self::ApprovalResolved {
                call_id,
                decision: decision.into(),
            },
            IssueEventBody::StageCompleted { stage } => Self::StageCompleted { stage },
            IssueEventBody::BudgetExhausted {
                spent_micros,
                limit_micros,
            } => Self::BudgetExhausted {
                spent_micros,
                limit_micros,
            },
            IssueEventBody::BudgetRestored {
                spent_micros,
                limit_micros,
            } => Self::BudgetRestored {
                spent_micros,
                limit_micros,
            },
            IssueEventBody::Comment { text } => Self::Comment { text },
            IssueEventBody::Opened => Self::Opened,
            IssueEventBody::Moved { from, to } => Self::Moved {
                from: from.into(),
                to: to.into(),
            },
            IssueEventBody::Assigned { from, to } => Self::Assigned {
                from: from.map(|id| agent_ref(&id, handles)),
                to: to.map(|id| agent_ref(&id, handles)),
            },
            IssueEventBody::RunStarted {
                attempt, trigger, ..
            } => Self::RunStarted {
                attempt,
                trigger: trigger.into(),
            },
            IssueEventBody::RunSettled {
                attempt,
                status,
                error,
                ..
            } => Self::RunSettled {
                attempt,
                status: status.into(),
                error,
            },
            IssueEventBody::Blocked { reason } => Self::Blocked { reason },
            IssueEventBody::Unblocked => Self::Unblocked,
            IssueEventBody::Cancelled => Self::Cancelled,
            IssueEventBody::WorktreeReclaimed { branch_deleted } => {
                Self::WorktreeReclaimed { branch_deleted }
            }
            IssueEventBody::WorktreeKept { reason } => Self::WorktreeKept { reason },
        }
    }
}

/// One entry on an issue's timeline.
#[derive(Debug, Serialize, ToSchema)]
pub struct IssueEventDto {
    pub id: String,
    pub number: i64,
    pub actor: ActorDto,
    pub body: IssueEventBodyDto,
    pub created_at_ms: i64,
}

impl IssueEventDto {
    fn with_handles(row: IssueEventRow, handles: &ActorHandles) -> Self {
        let actor = match &row.actor {
            IssueActor::User => ActorDto::User,
            IssueActor::System => ActorDto::System,
            IssueActor::Agent(id) => ActorDto::Agent(agent_ref(id, handles)),
        };
        Self {
            id: row.id.as_str().to_owned(),
            number: row.number,
            actor,
            body: IssueEventBodyDto::with_handles(row.body, handles),
            created_at_ms: row.created_at.timestamp_millis(),
        }
    }
}

/// A comment being posted.
#[derive(Debug, Deserialize, ToSchema)]
pub struct NewCommentBody {
    pub text: String,
}

/// One execution of an issue.
#[derive(Debug, Serialize, ToSchema)]
pub struct IssueRunDto {
    /// The issue this ran, by its per-project number.
    pub number: i64,
    /// 1 for an issue's first run, incrementing thereafter — how the
    /// execution log addresses it.
    pub attempt: i64,
    pub agent_id: String,
    pub status: RunStatusDto,
    pub trigger: RunTriggerDto,
    /// The session the run executed in. Present once claimed; it is what
    /// the trace viewer opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at_ms: Option<i64>,
}

impl From<IssueRunRow> for IssueRunDto {
    fn from(row: IssueRunRow) -> Self {
        Self {
            number: row.number,
            attempt: row.attempt,
            agent_id: row.agent_id.to_string(),
            status: row.status.into(),
            trigger: row.trigger.into(),
            session_id: row.session_id.map(|s| s.into_inner()),
            error: row.error,
            created_at_ms: row.created_at.timestamp_millis(),
            started_at_ms: row.started_at.map(|t| t.timestamp_millis()),
            settled_at_ms: row.settled_at.map(|t| t.timestamp_millis()),
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateProjectRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Absolute path to an existing git repository. Omit it and the server
    /// creates one under the workspace's `work/` directory instead.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Daily spend ceiling in micro-USD (USD × 10^6). Omit for no limit;
    /// `0` pauses the board's agents without archiving it. Integer, never a
    /// float — a budget compared with rounding error is a budget that
    /// disagrees with the ledger it is measured against.
    #[serde(default)]
    pub daily_budget_micros: Option<i64>,
    /// How many runs the board may start on its own, by promoting cards off
    /// the top of Todo. Omit for the default; `0` leaves every start to
    /// whoever drags the card.
    #[serde(default)]
    pub max_parallel_issue_runs: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateProjectRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Daily spend ceiling in micro-USD (USD × 10^6). Omit for no limit;
    /// `0` pauses the board's agents without archiving it. Integer, never a
    /// float — a budget compared with rounding error is a budget that
    /// disagrees with the ledger it is measured against.
    #[serde(default)]
    pub daily_budget_micros: Option<i64>,
    /// How many runs the board may start on its own, by promoting cards off
    /// the top of Todo. Full-replace like every other field here: omitting
    /// it restores the default rather than keeping what the board had.
    #[serde(default)]
    pub max_parallel_issue_runs: Option<i64>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SetArchivedRequest {
    pub archived: bool,
}

#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct ListProjectsQuery {
    /// Fold the archive back into the listing.
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateIssueRequest {
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Which column the card opens in. Defaults to the backlog.
    #[serde(default)]
    pub status: Option<IssueStatusDto>,
    #[serde(default)]
    pub priority: Option<IssuePriorityDto>,
    /// The agent to put on it. Required when opening straight into
    /// In Progress.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Open it as a step of that issue's number. One level only.
    #[serde(default)]
    pub parent: Option<i64>,
    /// Which barrier under the parent. Ignored without one.
    #[serde(default)]
    pub stage: Option<i64>,
}

/// Sparse patch: a field the body leaves out is left alone.
/// `blocked_reason` is doubly optional — an explicit `null` clears the
/// block, an absent key leaves it.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateIssueRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: Option<IssuePriorityDto>,
    /// An explicit `null` unassigns; an absent key leaves the assignee.
    #[serde(default, deserialize_with = "double_option")]
    pub assignee: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub blocked_reason: Option<Option<String>>,
    #[serde(default)]
    pub cancelled: Option<bool>,
    /// Re-parent by number; `0` detaches. Absent leaves the parent alone.
    #[serde(default)]
    pub parent: Option<i64>,
    #[serde(default)]
    pub stage: Option<i64>,
}

/// One drag-and-drop: where the card lands, plus that column's full
/// contents in their new order.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MoveIssueRequest {
    pub status: IssueStatusDto,
    /// Every issue number in the destination column, in order, including
    /// the one being moved.
    pub ordered_numbers: Vec<i64>,
}

#[utoipa::path(
    get,
    path = "/projects",
    tag = "projects",
    params(ListProjectsQuery),
    responses(
        (status = 200, description = "Projects, most recently touched first", body = inline(ListResponse<ProjectDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_projects(
    State(state): State<AdminState>,
    Query(query): Query<ListProjectsQuery>,
) -> Result<Json<ListResponse<ProjectDto>>> {
    let items = state
        .project_manager
        .list_projects(query.include_archived)
        .await
        .map_err(project_err)?
        .into_iter()
        .map(ProjectDto::from)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    post,
    path = "/projects",
    tag = "projects",
    request_body = CreateProjectRequest,
    responses(
        (status = 201, description = "The new project", body = ProjectDto),
        (status = 400, description = "Blank name, or a workdir that is relative, not a repo, or overlaps the workspace", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn create_project(
    State(state): State<AdminState>,
    Json(req): Json<CreateProjectRequest>,
) -> Result<(StatusCode, Json<ProjectDto>)> {
    let row = state
        .project_manager
        .create_project(NewProject {
            name: req.name,
            description: req.description,
            workdir: req.workdir,
            daily_budget: req
                .daily_budget_micros
                .map(baybo_model::MicroUsd::from_micros),
            max_parallel_issue_runs: req
                .max_parallel_issue_runs
                .map(parallel_issue_runs)
                .transpose()?,
        })
        .await
        .map_err(project_err)?;
    Ok((StatusCode::CREATED, Json(ProjectDto::from(row))))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    responses(
        (status = 200, description = "The project", body = ProjectDto),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
    )
)]
async fn get_project(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
) -> Result<Json<ProjectDto>> {
    let id = parse_project_id(&project_id)?;
    let row = state
        .project_manager
        .get_project(&id)
        .await
        .map_err(project_err)?;
    Ok(Json(ProjectDto::from(row)))
}

#[utoipa::path(
    put,
    path = "/projects/{project_id}",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    request_body = UpdateProjectRequest,
    responses(
        (status = 200, description = "The edited project", body = ProjectDto),
        (status = 400, description = "Blank name", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
        (status = 409, description = "The project is archived", body = ErrorBody),
    )
)]
async fn update_project(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
    Json(req): Json<UpdateProjectRequest>,
) -> Result<Json<ProjectDto>> {
    let id = parse_project_id(&project_id)?;
    let row = state
        .project_manager
        .update_project(
            &id,
            ProjectUpdate {
                name: req.name,
                description: req.description,
                daily_budget: req
                    .daily_budget_micros
                    .map(baybo_model::MicroUsd::from_micros),
                max_parallel_issue_runs: req
                    .max_parallel_issue_runs
                    .map(parallel_issue_runs)
                    .transpose()?
                    .unwrap_or(DEFAULT_MAX_PARALLEL_ISSUE_RUNS),
            },
        )
        .await
        .map_err(project_err)?;
    Ok(Json(ProjectDto::from(row)))
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/archive",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    request_body = SetArchivedRequest,
    responses(
        (status = 200, description = "The project, archived or restored", body = ProjectDto),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
    )
)]
async fn set_project_archived(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
    Json(req): Json<SetArchivedRequest>,
) -> Result<Json<ProjectDto>> {
    let id = parse_project_id(&project_id)?;
    let row = state
        .project_manager
        .set_project_archived(&id, req.archived)
        .await
        .map_err(project_err)?;
    Ok(Json(ProjectDto::from(row)))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/issues",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    responses(
        (status = 200, description = "The whole board, column by column, in order", body = inline(ListResponse<IssueDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
    )
)]
async fn list_issues(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
) -> Result<Json<ListResponse<IssueDto>>> {
    let id = parse_project_id(&project_id)?;
    let board = state
        .project_manager
        .list_issues(&id)
        .await
        .map_err(project_err)?;
    let items = board
        .iter()
        .cloned()
        .map(|row| IssueDto::on_board(row, &board))
        .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/issues",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    request_body = CreateIssueRequest,
    responses(
        (status = 201, description = "The new issue, numbered and placed", body = IssueDto),
        (status = 400, description = "Blank title", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
        (status = 409, description = "The project is archived", body = ErrorBody),
    )
)]
async fn create_issue(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
    Json(req): Json<CreateIssueRequest>,
) -> Result<(StatusCode, Json<IssueDto>)> {
    let id = parse_project_id(&project_id)?;
    let row = state
        .project_manager
        .create_issue(
            &id,
            IssueActor::User,
            NewIssueRequest {
                title: req.title,
                description: req.description,
                status: req.status.unwrap_or(IssueStatusDto::Backlog).into(),
                priority: req.priority.unwrap_or_default().into(),
                assignee: parse_assignee(req.assignee)?,
                parent: req.parent,
                stage: req.stage.unwrap_or(0),
                // The operator opening a card is opening it once, on
                // purpose. Dedupe keys are namespaced per scheduled job and
                // exist only there.
                source_key: None,
            },
        )
        .await
        .map_err(project_err)?;
    Ok((
        StatusCode::CREATED,
        Json(on_board(&state, &id, row.into_issue()).await?),
    ))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/issues/{number}",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
    ),
    responses(
        (status = 200, description = "The issue", body = IssueDto),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
    )
)]
async fn get_issue(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
) -> Result<Json<IssueDto>> {
    let id = parse_project_id(&project_id)?;
    let row = state
        .project_manager
        .get_issue(&id, number)
        .await
        .map_err(project_err)?;
    Ok(Json(on_board(&state, &id, row).await?))
}

#[utoipa::path(
    patch,
    path = "/projects/{project_id}/issues/{number}",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
    ),
    request_body = UpdateIssueRequest,
    responses(
        (status = 200, description = "The edited issue; omitted fields are unchanged", body = IssueDto),
        (status = 400, description = "Blank title, or a body that sets no field", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
        (status = 409, description = "The project is archived", body = ErrorBody),
    )
)]
async fn update_issue(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
    Json(req): Json<UpdateIssueRequest>,
) -> Result<Json<IssueDto>> {
    let id = parse_project_id(&project_id)?;
    // The wire addresses a parent by number, like every other reference on
    // this surface; the store wants an id. `0` detaches, which is the one
    // number no issue has.
    let parent = match req.parent {
        None => None,
        Some(0) => Some(None),
        Some(number) => Some(Some(
            state
                .project_manager
                .get_issue(&id, number)
                .await
                .map_err(project_err)?
                .id,
        )),
    };
    let row = state
        .project_manager
        .update_issue(
            &id,
            number,
            IssueActor::User,
            IssueUpdate {
                title: req.title,
                description: req.description,
                priority: req.priority.map(IssuePriority::from),
                assignee: req.assignee.map(parse_assignee).transpose()?,
                blocked_reason: req.blocked_reason,
                cancelled: req.cancelled,
                parent,
                stage: req.stage,
            },
        )
        .await
        .map_err(project_err)?;
    Ok(Json(on_board(&state, &id, row).await?))
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/issues/{number}/move",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
    ),
    request_body = MoveIssueRequest,
    responses(
        (status = 200, description = "The moved issue, in its new column and place", body = IssueDto),
        (status = 400, description = "The destination's contents don't include the moved issue", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
        (status = 409, description = "The project is archived", body = ErrorBody),
    )
)]
async fn move_issue(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
    Json(req): Json<MoveIssueRequest>,
) -> Result<Json<IssueDto>> {
    let id = parse_project_id(&project_id)?;
    let row = state
        .project_manager
        .move_issue(
            &id,
            number,
            IssueActor::User,
            req.status.into(),
            &req.ordered_numbers,
        )
        .await
        .map_err(project_err)?;
    Ok(Json(on_board(&state, &id, row).await?))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/issues/{number}/runs",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
    ),
    responses(
        (status = 200, description = "Every run of this issue, newest first", body = inline(ListResponse<IssueRunDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
    )
)]
async fn list_issue_runs(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
) -> Result<Json<ListResponse<IssueRunDto>>> {
    let id = parse_project_id(&project_id)?;
    let items = state
        .project_manager
        .list_runs(&id, number)
        .await
        .map_err(project_err)?
        .into_iter()
        .map(IssueRunDto::from)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

/// Query for `GET /projects/{project_id}/feed`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct FeedQuery {
    /// Page backwards from this instant (ms). Omit for the newest.
    #[serde(default)]
    pub before_ms: Option<i64>,
    /// How many entries. Clamped by the server.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Request body for resolving an approval from a card.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ResolveApprovalRequest {
    pub decision: ApprovalDecisionDto,
}

fn parked_approval_session(
    channel: &baybo_channels::Channel,
    call_id: &str,
) -> Option<baybo_model::SessionId> {
    channel.pending_approval_sessions().into_iter().find(|s| {
        channel
            .pending_approvals(s)
            .iter()
            .any(|req| req.call_id == call_id)
    })
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/issues/{number}/approvals/{call_id}",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
        ("call_id" = String, Path, description = "The approval's call id, from its timeline entry"),
    ),
    request_body = ResolveApprovalRequest,
    responses(
        (status = 204, description = "The prompt was answered"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue, or this card has no prompt waiting on that call", body = ErrorBody),
    )
)]
async fn resolve_approval(
    State(state): State<AdminState>,
    Path((project_id, number, call_id)): Path<(String, i64, String)>,
    Json(req): Json<ResolveApprovalRequest>,
) -> Result<StatusCode> {
    let id = parse_project_id(&project_id)?;
    // A point read, and its result is used: it 404s an unknown board or
    // card before the queue is touched, and the card's own id is what the
    // parked prompt is checked against below.
    let issue = state
        .project_manager
        .get_issue(&id, number)
        .await
        .map_err(project_err)?;
    let channel = state
        .channel_registry
        .get(&baybo_model::ChannelType::owner())
        .ok_or_else(|| GatewayError::NotFound("the owner channel".to_owned()))?;
    let raised_here = match parked_approval_session(&channel, &call_id) {
        Some(session) => state
            .session_manager
            .get(&session)
            .await
            .ok()
            .flatten()
            .and_then(|session| {
                session
                    .trigger
                    .issue()
                    .map(|(_, issue_id, _)| *issue_id == issue.id)
            })
            .unwrap_or(false),
        None => false,
    };
    let decision: baybo_model::ApprovalDecision = req.decision.into();
    let Some(session_id) = raised_here
        .then(|| channel.resolve_approval(&call_id, decision))
        .flatten()
    else {
        return Err(GatewayError::NotFound(format!(
            "no approval waiting on call {call_id}"
        )));
    };
    channel.dispatch_approval_resolved(call_id, session_id, decision);
    Ok(StatusCode::NO_CONTENT)
}

/// One board with something waiting on the operator.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectAttentionDto {
    pub project_id: String,
    pub name: String,
    /// Tool calls parked on an approval prompt.
    pub approvals: usize,
    /// Runs recorded but not started, because the board is over budget.
    pub held: usize,
    /// Live cards whose newest run failed.
    pub failed: usize,
    /// Agents' comments and cards arriving in Review since you last looked.
    pub unread: usize,
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/read",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    responses(
        (status = 204, description = "Noted; the board's unread count resets"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
    )
)]
async fn mark_project_read(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
) -> Result<StatusCode> {
    let id = parse_project_id(&project_id)?;
    state
        .project_manager
        .mark_read(&id)
        .await
        .map_err(project_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/projects/attention",
    tag = "projects",
    responses(
        (status = 200, description = "Boards with work stuck on the operator. Boards with nothing waiting are absent.", body = inline(ListResponse<ProjectAttentionDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn projects_attention(
    State(state): State<AdminState>,
) -> Result<Json<ListResponse<ProjectAttentionDto>>> {
    // One snapshot of the live approval queue, taken with no `.await`
    // between reading it and using it — the same discipline the chat list
    // uses, so a prompt cannot be counted after it has been answered.
    let pending: Vec<baybo_model::SessionId> = state
        .channel_registry
        .get(&baybo_model::ChannelType::owner())
        .map(|channel| channel.pending_approval_sessions().into_iter().collect())
        .unwrap_or_default();
    let counts = state
        .project_manager
        .attention(&pending)
        .await
        .map_err(project_err)?;
    // One listing rather than a `get_project` per row: this repaints on
    // every board change, and the set is small.
    let names: std::collections::HashMap<String, String> = state
        .project_manager
        .list_projects(false)
        .await
        .map_err(project_err)?
        .into_iter()
        .map(|row| (row.id.as_str().to_owned(), row.name))
        .collect();
    let items = counts
        .into_iter()
        .filter_map(|(project_id, count)| {
            let id = project_id.as_str().to_owned();
            names.get(&id).map(|name| ProjectAttentionDto {
                project_id: id.clone(),
                name: name.clone(),
                approvals: count.approvals,
                held: count.held,
                failed: count.failed,
                unread: count.unread,
            })
        })
        .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/feed",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id"), FeedQuery),
    responses(
        (status = 200, description = "This project's activity, newest first", body = inline(ListResponse<IssueEventDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
    )
)]
async fn project_feed(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
    Query(query): Query<FeedQuery>,
) -> Result<Json<ListResponse<IssueEventDto>>> {
    let id = parse_project_id(&project_id)?;
    let before = query
        .before_ms
        .map(|ms| {
            chrono::DateTime::from_timestamp_millis(ms)
                .ok_or_else(|| GatewayError::BadRequest(format!("before_ms out of range: {ms}")))
        })
        .transpose()?;
    let rows = state
        .project_manager
        .feed(
            &id,
            before,
            query.limit.unwrap_or(baybo_project::MAX_FEED_PAGE),
        )
        .await
        .map_err(project_err)?;
    let handles = actor_handles(&state, &id, &rows).await;
    let items = rows
        .into_iter()
        .map(|row| IssueEventDto::with_handles(row, &handles))
        .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/issues/{number}/events",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
    ),
    responses(
        (status = 200, description = "This issue's timeline, oldest first", body = inline(ListResponse<IssueEventDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
    )
)]
async fn list_issue_events(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
) -> Result<Json<ListResponse<IssueEventDto>>> {
    let id = parse_project_id(&project_id)?;
    let rows = state
        .project_manager
        .timeline(&id, number)
        .await
        .map_err(project_err)?;
    let handles = actor_handles(&state, &id, &rows).await;
    let items = rows
        .into_iter()
        .map(|row| IssueEventDto::with_handles(row, &handles))
        .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/issues/{number}/comments",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
    ),
    request_body = NewCommentBody,
    responses(
        (status = 200, description = "The recorded comment", body = IssueEventDto),
        (status = 400, description = "Empty comment", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
    )
)]
async fn create_comment(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
    Json(req): Json<NewCommentBody>,
) -> Result<Json<IssueEventDto>> {
    let id = parse_project_id(&project_id)?;
    let entry = state
        .project_manager
        .comment(&id, number, IssueActor::User, &req.text)
        .await
        .map_err(project_err)?;
    // No handles to resolve, and no lookup to spend on finding that out:
    // the actor is the operator by construction two lines up, and a comment
    // body names nobody. An empty map is the whole answer.
    Ok(Json(IssueEventDto::with_handles(
        entry,
        &ActorHandles::new(),
    )))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/runs",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    responses(
        (status = 200, description = "The board's unfinished runs — which cards are working", body = inline(ListResponse<IssueRunDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
    )
)]
async fn list_active_runs(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
) -> Result<Json<ListResponse<IssueRunDto>>> {
    let id = parse_project_id(&project_id)?;
    let items = state
        .project_manager
        .active_runs(&id)
        .await
        .map_err(project_err)?
        .into_iter()
        .map(IssueRunDto::from)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/issues/{number}/runs/cancel",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
    ),
    responses(
        (status = 204, description = "The run is stopping, or was never started and is now cancelled"),
        (status = 400, description = "Nothing is running on this issue", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
    )
)]
async fn cancel_run(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
) -> Result<StatusCode> {
    let id = parse_project_id(&project_id)?;
    let Some(session) = state
        .project_manager
        .cancel_run(&id, number)
        .await
        .map_err(project_err)?
    else {
        // It never started; the manager settled it.
        return Ok(StatusCode::NO_CONTENT);
    };

    // A run that is executing stops the way `/stop` stops a reply — the
    // waiter watching that turn is what settles the ledger row, so the two
    // never race to record an outcome.
    let turns = state
        .turn_lifecycle
        .list_active_chat_turns_by_session(&session)
        .await
        .map_err(|e| GatewayError::Internal(format!("issue run turns: {e}")))?;
    for turn in turns {
        state
            .turn_lifecycle
            .cancel(&turn.id, baybo_turn::CancelReason::OperatorCancel, vec![])
            .await
            .map_err(|e| GatewayError::Internal(format!("cancel issue run: {e}")))?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/issues/{number}/runs/retry",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
    ),
    responses(
        (status = 201, description = "The new run", body = IssueRunDto),
        (status = 400, description = "The issue has nobody on it, or the board has finished with it: a cancelled card has to be reopened and a done one moved back before it runs again", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
        (status = 409, description = "A run is already in flight, or the project is archived", body = ErrorBody),
    )
)]
async fn retry_run(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
) -> Result<(StatusCode, Json<IssueRunDto>)> {
    let id = parse_project_id(&project_id)?;
    let run = state
        .project_manager
        .retry_run(&id, number)
        .await
        .map_err(project_err)?;
    Ok((StatusCode::CREATED, Json(IssueRunDto::from(run))))
}
