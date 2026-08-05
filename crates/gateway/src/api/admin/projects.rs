//! `/v1/projects/*` — kanban projects and the issues on their boards
//! (docs/todo/kanban.md).
//!
//! An issue is addressed as `(project, number)` everywhere on this surface.
//! Its ULID exists for child tables to reference, and is deliberately not a
//! route parameter: a request that could name an issue without naming its
//! project would be a request that can reach across boards.

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
    IssueActor, IssueEventBody, IssueEventRow, IssuePriority, IssueRow, IssueRunRow, IssueStatus,
    IssueUpdate, ProjectRow, ProjectUpdate, RunStatus, RunTrigger,
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

/// One card, with the parent number and progress ring its board implies.
///
/// A second read of the board — the price of `parent` being a *number* on
/// the wire while the row carries an id. Worth it: every other reference on
/// this surface is a number, and a client that had to resolve ULIDs to draw
/// a card would need a second endpoint to do it.
async fn on_board(state: &AdminState, project: &ProjectId, row: IssueRow) -> Result<IssueDto> {
    let board = state
        .project_manager
        .list_issues(project)
        .await
        .map_err(project_err)?;
    Ok(IssueDto::on_board(row, &board))
}

/// Parse a path segment into a [`ProjectId`], running the same grammar the
/// filesystem depends on rather than trusting the URL.
pub(super) fn parse_project_id(raw: &str) -> Result<ProjectId> {
    ProjectId::parse(raw).map_err(|e| GatewayError::BadRequest(e.to_string()))
}

/// Parse an assignee, running the agent-id grammar rather than trusting
/// the body. The manager still checks the agent exists and can run.
fn parse_assignee(raw: Option<String>) -> Result<Option<AgentProfileId>> {
    raw.map(AgentProfileId::parse)
        .transpose()
        .map_err(|e| GatewayError::BadRequest(e.to_string()))
}

/// Deserialize a clearable optional field.
///
/// Plain `Option<Option<T>>` cannot express "explicitly null": serde folds
/// both a missing key and a `null` into the outer `None`. Wrapping the
/// parsed value in `Some` restores the distinction the patch semantics
/// need — absent leaves the field alone, `null` clears it.
fn double_option<'de, T, D>(deserializer: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::deserialize(deserializer).map(Some)
}

// ── DTOs ────────────────────────────────────────────────────────────────

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
    ///
    /// The parent number and the progress ring are both derived from
    /// `board` rather than queried: a list read already holds every issue
    /// in the project, so resolving them in memory costs nothing and
    /// cannot disagree with the rows beside it.
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

/// Why a run was started.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunTriggerDto {
    Started,
    Assigned,
    Retry,
    Comment,
    StageBarrier,
}

impl From<RunTrigger> for RunTriggerDto {
    fn from(trigger: RunTrigger) -> Self {
        match trigger {
            RunTrigger::Started => Self::Started,
            RunTrigger::Assigned => Self::Assigned,
            RunTrigger::Retry => Self::Retry,
            RunTrigger::Comment => Self::Comment,
            RunTrigger::StageBarrier => Self::StageBarrier,
        }
    }
}

/// What one timeline entry says.
///
/// A mirror of the store's `IssueEventBody` rather than the type itself,
/// like every other enum on this surface. Tagged on `kind`, so the client
/// gets a discriminated union it can exhaustively switch on — the whole
/// reason this is not a free-form payload.
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
        from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
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

impl From<IssueEventBody> for IssueEventBodyDto {
    fn from(body: IssueEventBody) -> Self {
        match body {
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
                from: from.map(|id| id.to_string()),
                to: to.map(|id| id.to_string()),
            },
            // The run id is deliberately dropped: the execution log in the
            // rail addresses a run by its attempt within this issue, and a
            // second identifier for the same thing is one the UI would have
            // to keep consistent for no reader's benefit.
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
    /// `"user"`, or the agent's id. Which of the two it is comes from
    /// [`Self::actor_is_agent`] rather than from parsing this.
    pub actor: String,
    pub actor_is_agent: bool,
    pub body: IssueEventBodyDto,
    pub created_at_ms: i64,
}

impl From<IssueEventRow> for IssueEventDto {
    fn from(row: IssueEventRow) -> Self {
        let (actor, actor_is_agent) = match &row.actor {
            IssueActor::User => ("user".to_owned(), false),
            IssueActor::Agent(id) => (id.to_string(), true),
        };
        Self {
            id: row.id.as_str().to_owned(),
            number: row.number,
            actor,
            actor_is_agent,
            body: row.body.into(),
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

// ── Projects ────────────────────────────────────────────────────────────

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

// ── Issues ──────────────────────────────────────────────────────────────

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
            },
        )
        .await
        .map_err(project_err)?;
    Ok((StatusCode::CREATED, Json(on_board(&state, &id, row).await?)))
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
    let items = state
        .project_manager
        .timeline(&id, number)
        .await
        .map_err(project_err)?
        .into_iter()
        .map(IssueEventDto::from)
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
    Ok(Json(IssueEventDto::from(entry)))
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
        (status = 400, description = "The issue has nobody on it", body = ErrorBody),
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
