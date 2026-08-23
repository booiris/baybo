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
use baybo_project::{AttachmentRequest, NewIssueRequest, NewProject, ProjectError};
use baybo_store::project::{
    BoardCards, DEFAULT_MAX_PARALLEL_ISSUE_RUNS, IssueActor, IssueAttachment, IssueEventBody,
    IssueEventRow, IssuePriority, IssueRow, IssueRunRow, IssueStatus, IssueUpdate, ProjectRow,
    ProjectUpdate, RunStatus, RunTrigger,
};

use super::chat::{ChatSessionDetail, GetSessionQuery, session_detail};
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
        .routes(routes!(issue_run_transcript))
        .routes(routes!(list_issue_events))
        .routes(routes!(project_feed))
        .routes(routes!(projects_attention))
        .routes(routes!(projects_activity))
        .routes(routes!(mark_issue_read))
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
        .board_cards(project)
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
    resolve_handles(state, project, ids).await
}

/// Look up the `@handle` for each id, dropping any that turns out to
/// belong to another board. One home for the question, because the feed
/// asks it of two different kinds of entry and a second spelling would be
/// free to answer differently about who is on this team.
async fn resolve_handles(
    state: &AdminState,
    project: &ProjectId,
    ids: impl IntoIterator<Item = AgentProfileId>,
) -> ActorHandles {
    let unique: std::collections::HashSet<AgentProfileId> = ids.into_iter().collect();
    futures::future::join_all(unique.into_iter().map(|id| async {
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
    /// Daily token ceiling in the same UTC window; absent means unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_budget_tokens: Option<i64>,
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
            daily_budget_tokens: row.daily_budget_tokens,
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

/// A file on a card — on its description, or on one comment.
///
/// No `kind`: which of image / file this is falls out of `mime_type`, and
/// the client is the only side that has to make that call (it decides
/// between a thumbnail and a chip). A stored discriminator would be a
/// second answer that could disagree with the bytes.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IssueAttachmentDto {
    /// Capability id from `POST /v1/blobs`. Possession is the read right,
    /// so this is as sensitive as the file it names.
    pub blob_id: String,
    pub mime_type: String,
    pub size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

impl From<IssueAttachment> for IssueAttachmentDto {
    fn from(a: IssueAttachment) -> Self {
        Self {
            blob_id: a.blob_id,
            mime_type: a.mime_type,
            size: a.size,
            filename: a.filename,
        }
    }
}

/// What a client may say about a file it is hanging on a card: which blob,
/// and what to call it. The type and the size are read off the store — see
/// `baybo_project::AttachmentRequest`, which this maps onto.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct IssueAttachmentRequest {
    /// Full blob id from `POST /v1/blobs`.
    pub blob_id: String,
    #[serde(default)]
    pub filename: Option<String>,
}

impl From<IssueAttachmentRequest> for AttachmentRequest {
    fn from(a: IssueAttachmentRequest) -> Self {
        Self {
            blob_id: a.blob_id,
            filename: a.filename,
        }
    }
}

fn requested(attachments: Vec<IssueAttachmentRequest>) -> Vec<AttachmentRequest> {
    attachments
        .into_iter()
        .map(AttachmentRequest::from)
        .collect()
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IssueDto {
    /// The human address, unique within its project: `#3`.
    pub number: i64,
    pub project_id: String,
    pub title: String,
    pub description: String,
    /// Files hung on the description.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<IssueAttachmentDto>,
    pub status: IssueStatusDto,
    pub priority: IssuePriorityDto,
    /// The agent on it, if any. In Progress always has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Rank within the column, dense and ascending.
    pub position: i64,
    /// Kept in front of the operator: a pinned card is read first in its
    /// column, above even the cards carrying something new. A reading
    /// order and nothing else — it never touches `position`, and the board
    /// does not take work out of Todo by it.
    pub pinned: bool,
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
    /// The card whose run filed this one, by its number on this board.
    /// Absent on a card nothing spun out of. Provenance and nothing else —
    /// it gates no work and orders no column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filed_from: Option<i64>,
    /// Which barrier under that parent. `0` and meaningless without one.
    pub stage: i64,
    /// This card's own steps, if it has any: how many are done out of how
    /// many still meant to happen. Cancelled steps leave both counts, so a
    /// card whose last steps were called off reads finished rather than
    /// stuck.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_issues: Option<SubIssueProgress>,
    /// What has happened on this card since the operator last opened it:
    /// agents' comments, and an agent moving it into Review. `0` on a card
    /// with nothing new — which is every card, a moment after it is read.
    pub unread: i64,
    /// This card's newest run failed and the card is still live. The board
    /// shows it, because a failure that leaves the card looking untouched
    /// is a badge pointing at something the operator cannot find.
    pub last_run_failed: bool,
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
    /// Takes the whole [`BoardCards`] rather than the rows, so the two
    /// signals arrive already resolved. A caller handed the rows and the
    /// runs would answer "did this card's newest run fail" a second time,
    /// and the board's badge and the card's face would then be two
    /// answers to one question.
    pub fn on_board(row: IssueRow, board: &BoardCards) -> Self {
        let signals = board.signals(&row.id);
        let number_of = |id: &baybo_model::IssueId| {
            board
                .rows
                .iter()
                .find(|issue| &issue.id == id)
                .map(|issue| issue.number)
        };
        let parent = row.parent_issue_id.as_ref().and_then(&number_of);
        let filed_from = row.filed_from.as_ref().and_then(&number_of);
        let children: Vec<IssueRow> = board
            .rows
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
            filed_from,
            sub_issues,
            unread: signals.unread as i64,
            last_run_failed: signals.last_run_failed,
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
            attachments: row
                .attachments
                .into_iter()
                .map(IssueAttachmentDto::from)
                .collect(),
            status: row.status.into(),
            priority: row.priority.into(),
            assignee: row.assignee.map(|a| a.to_string()),
            position: row.position,
            pinned: row.pinned,
            branch: row.branch,
            blocked_reason: row.blocked_reason,
            parent: None,
            filed_from: None,
            stage: row.stage,
            sub_issues: None,
            // A card built without its board is a card nobody is looking
            // at yet — the two signals are derived over the board, and
            // guessing them here is how they would drift.
            unread: 0,
            last_run_failed: false,
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
    Review,
    Stalled,
    Blocked,
    Grooming,
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
            RunTrigger::Review => Self::Review,
            RunTrigger::Stalled => Self::Stalled,
            RunTrigger::Blocked => Self::Blocked,
            RunTrigger::Grooming => Self::Grooming,
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

/// Mirror of [`baybo_model::ApprovalResolution`]: how the prompt resolved —
/// a decision, an expired window, an abandoned prompt, or standing policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolutionDto {
    Answered,
    TimedOut,
    Abandoned,
    Policy,
}

impl From<baybo_model::ApprovalResolution> for ApprovalResolutionDto {
    fn from(resolution: baybo_model::ApprovalResolution) -> Self {
        match resolution {
            baybo_model::ApprovalResolution::Answered => Self::Answered,
            baybo_model::ApprovalResolution::TimedOut => Self::TimedOut,
            baybo_model::ApprovalResolution::Abandoned => Self::Abandoned,
            baybo_model::ApprovalResolution::Policy => Self::Policy,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<IssueAttachmentDto>,
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
    RunInterrupted {
        attempt: i64,
        resumes: i64,
    },
    RunSettled {
        attempt: i64,
        status: RunStatusDto,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    RunRefused {
        trigger: RunTriggerDto,
        /// The attempt holding the card's slot. Absent when the row could
        /// not be read back.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<i64>,
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
        /// Which run is parked, by attempt number. Absent when the card was
        /// not recording it yet, or when no run owns the prompt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<i64>,
        tool: String,
        summary: String,
    },
    ApprovalResolved {
        call_id: String,
        decision: ApprovalDecisionDto,
        resolution: ApprovalResolutionDto,
    },
    StageCompleted {
        stage: i64,
    },
    Filed {
        number: i64,
    },
    BudgetExhausted {
        spent_micros: i64,
        limit_micros: i64,
    },
    BudgetRestored {
        spent_micros: i64,
        limit_micros: i64,
    },
    TokenBudgetExhausted {
        spent_tokens: i64,
        limit_tokens: i64,
    },
    TokenBudgetRestored {
        spent_tokens: i64,
        limit_tokens: i64,
    },
}

impl IssueEventBodyDto {
    fn with_handles(body: IssueEventBody, handles: &ActorHandles) -> Self {
        match body {
            IssueEventBody::ApprovalRequested {
                call_id,
                attempt,
                tool,
                summary,
            } => Self::ApprovalRequested {
                call_id,
                attempt,
                tool,
                summary,
            },
            IssueEventBody::ApprovalResolved {
                call_id,
                decision,
                resolution,
            } => Self::ApprovalResolved {
                call_id,
                decision: decision.into(),
                resolution: resolution.into(),
            },
            IssueEventBody::StageCompleted { stage } => Self::StageCompleted { stage },
            IssueEventBody::RunRefused { trigger, attempt } => Self::RunRefused {
                trigger: trigger.into(),
                attempt,
            },
            IssueEventBody::Filed { number } => Self::Filed { number },
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
            IssueEventBody::TokenBudgetExhausted {
                spent_tokens,
                limit_tokens,
            } => Self::TokenBudgetExhausted {
                spent_tokens,
                limit_tokens,
            },
            IssueEventBody::TokenBudgetRestored {
                spent_tokens,
                limit_tokens,
            } => Self::TokenBudgetRestored {
                spent_tokens,
                limit_tokens,
            },
            IssueEventBody::Comment { text, attachments } => Self::Comment {
                text,
                attachments: attachments
                    .into_iter()
                    .map(IssueAttachmentDto::from)
                    .collect(),
            },
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
            IssueEventBody::RunInterrupted {
                attempt, resumes, ..
            } => Self::RunInterrupted { attempt, resumes },
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

/// One line of a board's activity feed. Shaped like a timeline entry
/// because most lines are one, but `number` is optional: joining the team
/// is a fact about the board, not about any card, so there is nothing for
/// it to point at.
#[derive(Debug, Serialize, ToSchema)]
pub struct FeedEntryDto {
    /// The card this concerns. Absent on board-level entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<i64>,
    pub actor: ActorDto,
    pub body: FeedBodyDto,
    pub created_at_ms: i64,
    /// How long the run took, on a line that settled one.
    ///
    /// Lives on the feed entry rather than in the stored event because it is
    /// **derived** over the run's cost window, like the execution log's
    /// numbers and by the same query — a copy frozen into the timeline entry
    /// would be written before the run's last cost record necessarily is.
    /// Absent on every other kind of line, and on a settled run whose row is
    /// gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    /// What the run cost, in micro-USD. **Absent, not zero** — a run that
    /// billed nothing is a real answer and must not read as "unpriced".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<i64>,
}

/// A feed line's payload: either a card's timeline entry, or one of the
/// board-level facts that has no card to live on.
#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum FeedBodyDto {
    Card(IssueEventBodyDto),
    Board(BoardEventDto),
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BoardEventDto {
    /// Somebody joined the team. Derived from the roster, never stored as
    /// an event, so it cannot drift from who is actually on the board.
    Hired { agent: AgentRefDto },
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
    /// May be empty when `attachments` is not: "here, look at this" with a
    /// screenshot under it is a real thing to say on a card.
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<IssueAttachmentRequest>,
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
    /// What this run's LLM calls cost, in micro-USD. Derived from the cost
    /// ledger over the run's own window, so a session several runs share
    /// still bills each call to the run that made it.
    ///
    /// **Absent, not zero**, on responses that do not price runs — the
    /// board's active-run poll among them. Zero is a real answer there
    /// (a run that has not billed yet), so the two states cannot share an
    /// encoding without the board reporting free work as fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
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
            cost_micros: None,
            input_tokens: None,
            output_tokens: None,
        }
    }
}

/// A card's execution log with its totals attached. The totals ship beside
/// the rows rather than being left to the client because a client that
/// summed them would be the second place that decides what a card cost —
/// and the first to disagree once a run is filtered out of the view.
#[derive(Debug, Serialize, ToSchema)]
pub struct IssueRunLogDto {
    pub items: Vec<IssueRunDto>,
    /// Across **every** run, cancelled ones included: the money was spent
    /// either way, and a total that skipped them would not reconcile
    /// against the board's budget.
    pub total_cost_micros: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
}

impl From<baybo_project::RunWithSpend> for IssueRunDto {
    fn from(row: baybo_project::RunWithSpend) -> Self {
        Self {
            cost_micros: Some(row.spend.cost.into_micros()),
            input_tokens: Some(row.spend.input_tokens),
            output_tokens: Some(row.spend.output_tokens),
            ..Self::from(row.run)
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
    /// Daily token ceiling; omit for unlimited, or use `0` to pause agents.
    #[serde(default)]
    pub daily_budget_tokens: Option<i64>,
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
    /// Daily token ceiling; omit for unlimited, or use `0` to pause agents.
    #[serde(default)]
    pub daily_budget_tokens: Option<i64>,
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
    /// Files to hang on the description, uploaded to `POST /v1/blobs`
    /// first. Refused if a blob id names nothing the store has.
    #[serde(default)]
    pub attachments: Vec<IssueAttachmentRequest>,
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
    /// Full replace of the description's files: the list to end up with.
    /// An absent key leaves them alone, `[]` removes them all.
    #[serde(default)]
    pub attachments: Option<Vec<IssueAttachmentRequest>>,
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
    /// Keep this card at the top of its column, or stop. Singly optional:
    /// the pin is on or off, and an absent key leaves it.
    #[serde(default)]
    pub pinned: Option<bool>,
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
            daily_budget_tokens: req.daily_budget_tokens,
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
                daily_budget_tokens: req.daily_budget_tokens,
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
        .board_cards(&id)
        .await
        .map_err(project_err)?;
    let items = board
        .rows
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
                attachments: requested(req.attachments),
                status: req.status.unwrap_or(IssueStatusDto::Backlog).into(),
                priority: req.priority.unwrap_or_default().into(),
                assignee: parse_assignee(req.assignee)?,
                parent: req.parent,
                stage: req.stage.unwrap_or(0),
                // The operator opening a card is opening it once, on
                // purpose. Dedupe keys are namespaced per scheduled job and
                // exist only there.
                source_key: None,
                filed_from: None,
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
    let attachments = req.attachments.map(requested);
    let row = state
        .project_manager
        .update_issue(
            &id,
            number,
            IssueActor::User,
            IssueUpdate {
                title: req.title,
                description: req.description,
                // Left unset on purpose: the manager fills it from the
                // blob ids passed beside this patch, which is the only
                // door a file gets onto a card through.
                attachments: None,
                priority: req.priority.map(IssuePriority::from),
                assignee: req.assignee.map(parse_assignee).transpose()?,
                blocked_reason: req.blocked_reason,
                cancelled: req.cancelled,
                parent,
                stage: req.stage,
                pinned: req.pinned,
            },
            attachments.as_deref(),
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
        (status = 200, description = "Every run of this issue, newest first, priced", body = IssueRunLogDto),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
    )
)]
async fn list_issue_runs(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
) -> Result<Json<IssueRunLogDto>> {
    let id = parse_project_id(&project_id)?;
    let log = state
        .project_manager
        .run_log(&id, number)
        .await
        .map_err(project_err)?;
    Ok(Json(IssueRunLogDto {
        items: log.runs.into_iter().map(IssueRunDto::from).collect(),
        total_cost_micros: log.total.cost.into_micros(),
        total_input_tokens: log.total.input_tokens,
        total_output_tokens: log.total.output_tokens,
    }))
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/issues/{number}/runs/{attempt}/transcript",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number within the project"),
        (
            "attempt" = i64,
            Path,
            description = "Which run of the card, as the execution log numbers it"
        ),
        GetSessionQuery,
    ),
    responses(
        (
            status = 200,
            description = "The conversation this run worked in, newest page first. A SESSION, not a run: one agent's runs on a card share it, so a later attempt's page also holds the earlier ones",
            body = ChatSessionDetail
        ),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (
            status = 404,
            description = "Unknown project, issue or attempt — or a run no executor ever claimed, which has no conversation to show",
            body = ErrorBody
        ),
    )
)]
async fn issue_run_transcript(
    State(state): State<AdminState>,
    Path((project_id, number, attempt)): Path<(String, i64, i64)>,
    Query(query): Query<GetSessionQuery>,
) -> Result<Json<ChatSessionDetail>> {
    let id = parse_project_id(&project_id)?;
    let runs = state
        .project_manager
        .list_runs(&id, number)
        .await
        .map_err(project_err)?;
    let run = runs
        .iter()
        .find(|run| run.attempt == attempt)
        .ok_or_else(|| GatewayError::NotFound(format!("issue #{number} run #{attempt}")))?;
    // A run the executor never claimed has a ledger row and no session: the
    // card can say it is queued while there is still nothing to read.
    let sid = run.session_id.clone().ok_or_else(|| {
        GatewayError::NotFound(format!(
            "issue #{number} run #{attempt} has not started working"
        ))
    })?;
    let session = state
        .session_manager
        .get(&sid)
        .await
        .map_err(|e| GatewayError::Internal(format!("load run session: {e}")))?
        .ok_or_else(|| GatewayError::NotFound(format!("session {sid}")))?;
    session_detail(
        &state,
        sid.clone(),
        session,
        sid.to_string(),
        query.before_ordinal,
        query.limit,
    )
    .await
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
///
/// Every count here is an event. Runs the daily ceiling is holding are a
/// standing condition rather than news and are deliberately absent — the
/// board reports those in its own header, next to the setting that lifts
/// them. See `AttentionCounts`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectAttentionDto {
    pub project_id: String,
    pub name: String,
    /// Tool calls parked on an approval prompt.
    pub approvals: usize,
    /// Live cards whose newest run failed.
    pub failed: usize,
    /// Agents' comments and cards arriving in Review since you last looked.
    pub unread: usize,
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/issues/{number}/read",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("number" = i64, Path, description = "Issue number"),
    ),
    responses(
        (status = 204, description = "Noted; this card's unread count resets"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project or issue", body = ErrorBody),
    )
)]
async fn mark_issue_read(
    State(state): State<AdminState>,
    Path((project_id, number)): Path<(String, i64)>,
) -> Result<StatusCode> {
    let id = parse_project_id(&project_id)?;
    state
        .project_manager
        .mark_issue_read(&id, number)
        .await
        .map_err(project_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/read",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    responses(
        (status = 204, description = "Noted; every card on this board reads zero"),
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
        .mark_project_read(&id)
        .await
        .map_err(project_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// What one board is doing right now, for the switcher's dropdown.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectActivityDto {
    pub project_id: String,
    /// Runs executing. Counted from runs, not from the In Progress column —
    /// a run outlives its column, so the two disagree by design.
    pub working: usize,
    /// Spend since `since_ms`, in micro-USD. Shown against the ceiling that
    /// stops the board, so the caller must send the **budget's** day —
    /// `budget::day_start`, UTC midnight — or the pair in the dropdown
    /// accuses the board of crossing a ceiling it did not.
    pub burn_micros: i64,
    /// Tokens spent over the same window.
    pub burn_tokens: i64,
}

#[utoipa::path(
    get,
    path = "/projects/activity",
    tag = "projects",
    params(("since_ms" = Option<i64>, Query, description = "Start of the budget's day (UTC midnight), epoch ms. Defaults to the last 24 hours.")),
    responses(
        (status = 200, description = "Every board's live working count and today's burn. Boards with neither are absent.", body = inline(ListResponse<ProjectActivityDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn projects_activity(
    State(state): State<AdminState>,
    Query(query): Query<ActivityQuery>,
) -> Result<Json<ListResponse<ProjectActivityDto>>> {
    // The window is the caller's to name, because this endpoint also
    // answers "the last day" for surfaces with no ceiling in view. What it
    // must NOT be is the reader's calendar day: the number is read against
    // `daily_budget_*`, and `budget::day_start` is UTC.
    let since = match query.since_ms {
        Some(ms) => chrono::DateTime::from_timestamp_millis(ms)
            .ok_or_else(|| GatewayError::BadRequest(format!("since_ms out of range: {ms}")))?,
        None => chrono::Utc::now() - chrono::Duration::hours(24),
    };
    let items = state
        .project_manager
        .board_activity(since)
        .await
        .map_err(project_err)?
        .into_iter()
        .map(|(project_id, activity)| ProjectActivityDto {
            project_id: project_id.as_str().to_owned(),
            working: activity.working,
            burn_micros: activity.burn.cost.into_micros(),
            burn_tokens: activity.burn.tokens(),
        })
        .collect();
    Ok(Json(ListResponse::new(items)))
}

/// Query for `GET /projects/activity`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct ActivityQuery {
    #[serde(default)]
    pub since_ms: Option<i64>,
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
        (status = 200, description = "This project's activity, newest first", body = inline(ListResponse<FeedEntryDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
    )
)]
async fn project_feed(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
    Query(query): Query<FeedQuery>,
) -> Result<Json<ListResponse<FeedEntryDto>>> {
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
    let cards: Vec<IssueEventRow> = rows
        .iter()
        .filter_map(|entry| match entry {
            baybo_project::FeedEntry::Card { row, .. } => Some(row.clone()),
            baybo_project::FeedEntry::Hired { .. } => None,
        })
        .collect();
    let mut handles = actor_handles(&state, &id, &cards).await;
    // A hire names two agents the card entries never mention — the new
    // teammate, and whoever hired them.
    let mut extra: Vec<AgentProfileId> = Vec::new();
    for entry in &rows {
        if let baybo_project::FeedEntry::Hired {
            agent, hired_by, ..
        } = entry
        {
            extra.push(agent.clone());
            extra.extend(hired_by.clone());
        }
    }
    handles.extend(resolve_handles(&state, &id, extra).await);
    let items = rows
        .into_iter()
        .map(|entry| match entry {
            baybo_project::FeedEntry::Card { row, settled } => {
                let dto = IssueEventDto::with_handles(row, &handles);
                FeedEntryDto {
                    number: Some(dto.number),
                    actor: dto.actor,
                    body: FeedBodyDto::Card(dto.body),
                    created_at_ms: dto.created_at_ms,
                    duration_ms: settled.as_ref().and_then(|facts| facts.duration_ms),
                    cost_micros: settled.as_ref().map(|facts| facts.cost_micros),
                }
            }
            baybo_project::FeedEntry::Hired {
                agent,
                hired_by,
                at,
            } => FeedEntryDto {
                number: None,
                // Who did the hiring is the actor; the lead arrives with
                // the board and so reads as the system's doing.
                actor: match hired_by {
                    Some(id) => ActorDto::Agent(agent_ref(&id, &handles)),
                    None => ActorDto::User,
                },
                body: FeedBodyDto::Board(BoardEventDto::Hired {
                    agent: agent_ref(&agent, &handles),
                }),
                created_at_ms: at.timestamp_millis(),
                duration_ms: None,
                cost_micros: None,
            },
        })
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
        .comment(
            &id,
            number,
            IssueActor::User,
            &req.text,
            &requested(req.attachments),
        )
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
    // A run that is executing stops the way `/stop` stops a reply — the
    // waiter watching that turn is what settles the ledger row, so the two
    // never race to record an outcome. The board owns both halves; this
    // route only reports that it asked.
    state
        .project_manager
        .cancel_run(&id, number)
        .await
        .map_err(project_err)?;
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
        (status = 400, description = "The issue has nobody on it, a block has stopped it, or the board has finished with it: a blocked card has to be unblocked, a cancelled one reopened and a done one moved back before it runs again", body = ErrorBody),
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
