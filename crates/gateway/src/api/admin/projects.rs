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
fn project_err(e: ProjectError) -> GatewayError {
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

/// Parse a path segment into a [`ProjectId`], running the same grammar the
/// filesystem depends on rather than trusting the URL.
fn parse_project_id(raw: &str) -> Result<ProjectId> {
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
    /// Why work stopped. A badge on the card — blocked work stays in
    /// whichever column it was in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// Present once the issue is cancelled. The row is never deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
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
            blocked_reason: row.blocked_reason,
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
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl From<RunStatus> for RunStatusDto {
    fn from(status: RunStatus) -> Self {
        match status {
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
}

impl From<RunTrigger> for RunTriggerDto {
    fn from(trigger: RunTrigger) -> Self {
        match trigger {
            RunTrigger::Started => Self::Started,
            RunTrigger::Assigned => Self::Assigned,
            RunTrigger::Retry => Self::Retry,
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
}

impl From<IssueEventBody> for IssueEventBodyDto {
    fn from(body: IssueEventBody) -> Self {
        match body {
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
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateProjectRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
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
    let items = state
        .project_manager
        .list_issues(&id)
        .await
        .map_err(project_err)?
        .into_iter()
        .map(IssueDto::from)
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
            },
        )
        .await
        .map_err(project_err)?;
    Ok((StatusCode::CREATED, Json(IssueDto::from(row))))
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
    Ok(Json(IssueDto::from(row)))
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
            },
        )
        .await
        .map_err(project_err)?;
    Ok(Json(IssueDto::from(row)))
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
    Ok(Json(IssueDto::from(row)))
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
