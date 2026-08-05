//! Persistence interface for kanban projects and the issues on their board.
//!
//! A **project** is the container: a workdir, a team, and one board. An
//! **issue** is the unit of work on that board. Issues are numbered
//! per-project — `#3` means one thing inside one project — and the number
//! is assigned inside the insert transaction, never by the caller.
//!
//! Neither entity is ever hard-deleted from a production path: a project
//! archives (`archived_at`, the `cron_jobs`/`deck_cards` pattern) and an
//! issue cancels (`cancelled_at`). Issues carry conversation history, so
//! the session-data-is-core rule in `CLAUDE.md` covers them too.

use async_trait::async_trait;
use baybo_model::{AgentProfileId, IssueEventId, IssueId, IssueRunId, ProjectId, SessionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Which column an issue sits in. The set is fixed: entering
/// [`IssueStatus::InProgress`] is the single execution trigger, so a
/// user-definable column would be a user-definable trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueStatus {
    Backlog,
    Todo,
    InProgress,
    Review,
    Done,
}

impl IssueStatus {
    /// Board order, left to right. Also the order a list read returns.
    pub const ALL: [IssueStatus; 5] = [
        IssueStatus::Backlog,
        IssueStatus::Todo,
        IssueStatus::InProgress,
        IssueStatus::Review,
        IssueStatus::Done,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            IssueStatus::Backlog => "backlog",
            IssueStatus::Todo => "todo",
            IssueStatus::InProgress => "in_progress",
            IssueStatus::Review => "review",
            IssueStatus::Done => "done",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "backlog" => Some(IssueStatus::Backlog),
            "todo" => Some(IssueStatus::Todo),
            "in_progress" => Some(IssueStatus::InProgress),
            "review" => Some(IssueStatus::Review),
            "done" => Some(IssueStatus::Done),
            _ => None,
        }
    }
}

/// How urgent an issue is. Informs the lead's triage and the card face; it
/// never reorders the board on its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IssuePriority {
    Urgent,
    High,
    Medium,
    Low,
    #[default]
    None,
}

impl IssuePriority {
    pub fn as_str(self) -> &'static str {
        match self {
            IssuePriority::Urgent => "urgent",
            IssuePriority::High => "high",
            IssuePriority::Medium => "medium",
            IssuePriority::Low => "low",
            IssuePriority::None => "none",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "urgent" => Some(IssuePriority::Urgent),
            "high" => Some(IssuePriority::High),
            "medium" => Some(IssuePriority::Medium),
            "low" => Some(IssuePriority::Low),
            "none" => Some(IssuePriority::None),
            _ => None,
        }
    }
}

/// One row of `projects`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub id: ProjectId,
    pub name: String,
    pub description: String,
    /// Absolute path to the git repository this project's agents work in.
    /// Always set once the row exists — an empty workdir is materialised
    /// under `work/<name>/` at create time rather than left `NULL`, so no
    /// later reader has to handle a project with nowhere to work.
    pub workdir: String,
    /// Soft archive. There is no hard delete in any production path.
    pub archived_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Editable project content. Workdir is not here — it is set once, at
/// create, and re-pointing a live project is a separate decision from
/// renaming one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectUpdate {
    pub name: String,
    pub description: String,
}

/// One row of `issues`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRow {
    pub id: IssueId,
    pub project_id: ProjectId,
    /// Per-project, 1-based. The human address: `#3`.
    pub number: i64,
    pub title: String,
    pub description: String,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    /// Who is on it. `None` is unclaimed work — which is also why an
    /// unassigned issue cannot enter In Progress: a card in flight that
    /// nobody is on is a lie the board would keep telling.
    pub assignee: Option<AgentProfileId>,
    /// Manual order within `status`, ascending and dense: a move renumbers
    /// the whole target column in one transaction (the `reorder` shape),
    /// so positions never drift and never collide.
    pub position: i64,
    /// Why work stopped, when it did. A badge on the card, not a column —
    /// blocked work is still in whichever column it was in.
    pub blocked_reason: Option<String>,
    /// The terminal negative. A cancelled issue keeps its row, its number
    /// and its history; it just stops counting as live work.
    pub cancelled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Sparse issue edit: `None` leaves the field alone. `blocked_reason` is
/// doubly optional on purpose — `Some(None)` clears the block, `None`
/// leaves it as it was.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub priority: Option<IssuePriority>,
    /// `Some(None)` unassigns; `None` leaves the assignee alone.
    pub assignee: Option<Option<AgentProfileId>>,
    pub blocked_reason: Option<Option<String>>,
    pub cancelled: Option<bool>,
}

impl IssueUpdate {
    /// Whether this patch would change anything. A body that sets no field
    /// is a caller mistake worth a 400, not a silent no-op write.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.priority.is_none()
            && self.assignee.is_none()
            && self.blocked_reason.is_none()
            && self.cancelled.is_none()
    }
}

/// A new issue, before the store assigns its number and position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIssue {
    pub id: IssueId,
    pub project_id: ProjectId,
    pub title: String,
    pub description: String,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    pub assignee: Option<AgentProfileId>,
    pub created_at: DateTime<Utc>,
}

/// Who did the thing a timeline entry records.
///
/// Two kinds and no more: the operator working the board, and one of the
/// project's agents. A string here would let a caller invent a third.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueActor {
    User,
    Agent(AgentProfileId),
}

impl IssueActor {
    /// Storage form. Agents are prefixed rather than stored bare so the
    /// two cases stay distinguishable even if an agent is ever named
    /// `user`.
    pub fn to_storage(&self) -> String {
        match self {
            IssueActor::User => "user".to_owned(),
            IssueActor::Agent(id) => format!("agent:{}", id.as_str()),
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(IssueActor::User),
            other => other
                .strip_prefix("agent:")
                .and_then(|id| AgentProfileId::parse(id.to_owned()).ok())
                .map(IssueActor::Agent),
        }
    }
}

/// What one timeline entry says.
///
/// A tagged enum rather than a `kind` string beside a free-form payload:
/// every reader — the detail page, the activity feed, the brief assembled
/// for the next run — has to agree on what a "run settled" entry carries,
/// and the place to write that down once is here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IssueEventBody {
    /// Said by a person or an agent. The only entry a human writes.
    Comment {
        text: String,
    },
    Opened,
    Moved {
        from: IssueStatus,
        to: IssueStatus,
    },
    Assigned {
        from: Option<AgentProfileId>,
        to: Option<AgentProfileId>,
    },
    RunStarted {
        run_id: IssueRunId,
        attempt: i64,
        trigger: RunTrigger,
    },
    RunSettled {
        run_id: IssueRunId,
        attempt: i64,
        status: RunStatus,
        error: Option<String>,
    },
    Blocked {
        reason: String,
    },
    Unblocked,
    Cancelled,
}

impl IssueEventBody {
    /// Discriminator persisted alongside the body so a query can filter by
    /// kind without parsing every row's JSON. Derived from the variant, so
    /// the column can never disagree with the payload it sits next to.
    pub fn kind(&self) -> &'static str {
        match self {
            IssueEventBody::Comment { .. } => "comment",
            IssueEventBody::Opened => "opened",
            IssueEventBody::Moved { .. } => "moved",
            IssueEventBody::Assigned { .. } => "assigned",
            IssueEventBody::RunStarted { .. } => "run_started",
            IssueEventBody::RunSettled { .. } => "run_settled",
            IssueEventBody::Blocked { .. } => "blocked",
            IssueEventBody::Unblocked => "unblocked",
            IssueEventBody::Cancelled => "cancelled",
        }
    }
}

/// One entry on an issue's timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueEventRow {
    pub id: IssueEventId,
    pub issue_id: IssueId,
    pub project_id: ProjectId,
    pub number: i64,
    pub actor: IssueActor,
    pub body: IssueEventBody,
    pub created_at: DateTime<Utc>,
}

/// What a caller supplies to append to a timeline.
#[derive(Debug, Clone)]
pub struct NewIssueEvent {
    pub issue_id: IssueId,
    pub project_id: ProjectId,
    pub number: i64,
    pub actor: IssueActor,
    pub body: IssueEventBody,
}

#[async_trait]
pub trait ProjectStore: Send + Sync {
    /// Every project, newest activity first. `include_archived` folds the
    /// recycle bin back in.
    async fn list_projects(&self, include_archived: bool) -> Result<Vec<ProjectRow>>;

    /// Fetch one project, or `None` if it doesn't exist.
    async fn get_project(&self, id: &ProjectId) -> Result<Option<ProjectRow>>;

    /// Insert a project row verbatim. The workdir it names must already
    /// exist — materialising it is the domain layer's job, and it happens
    /// first so a crash leaves an unreferenced directory rather than a row
    /// pointing at nothing.
    async fn create_project(&self, row: &ProjectRow) -> Result<()>;

    /// Rename / re-describe. `Ok(false)` if no row matched.
    async fn update_project(&self, id: &ProjectId, update: &ProjectUpdate) -> Result<bool>;

    /// Stamp or clear `archived_at`. `Ok(false)` if no row matched.
    async fn set_project_archived(&self, id: &ProjectId, archived: bool) -> Result<bool>;

    /// Every issue on one board, ordered by status then position — the
    /// order the columns render in, so the caller needs no second sort.
    async fn list_issues(&self, project: &ProjectId) -> Result<Vec<IssueRow>>;

    /// Fetch one issue by its human address. Scoped by construction: there
    /// is no way to name an issue without naming its project.
    async fn get_issue(&self, project: &ProjectId, number: i64) -> Result<Option<IssueRow>>;

    /// Insert an issue, assigning its per-project `number` and its tail
    /// `position` **inside** the transaction. A caller-computed number is
    /// a race: two creates would read the same maximum and the second
    /// would trip the uniqueness constraint.
    async fn create_issue(&self, new: &NewIssue) -> Result<IssueRow>;

    /// Apply a sparse patch. `Ok(false)` if no row matched.
    async fn update_issue(
        &self,
        project: &ProjectId,
        number: i64,
        update: &IssueUpdate,
    ) -> Result<bool>;

    /// Move one issue into `status` and renumber that column to
    /// `ordered_numbers`, in one transaction so a partial move never
    /// lands. Ids outside the project are ignored rather than adopted.
    /// `Ok(false)` if the moved issue doesn't exist.
    async fn move_issue(
        &self,
        project: &ProjectId,
        number: i64,
        status: IssueStatus,
        ordered_numbers: &[i64],
    ) -> Result<bool>;

    /// Record a run before anything is dispatched — the ledger's whole
    /// point. The attempt number is assigned inside the insert, and the
    /// per-issue live index rejects a second unfinished run with
    /// [`StorageError::Conflict`], which callers read as "already working
    /// on it" rather than a failure.
    async fn enqueue_run(&self, new: &NewIssueRun) -> Result<IssueRunRow>;

    /// Append to an issue's timeline. Returns the stored row so a caller
    /// can announce exactly what a reader will fetch.
    async fn append_event(&self, new: &NewIssueEvent) -> Result<IssueEventRow>;

    /// One issue's timeline, oldest first — reading order, and the order
    /// the next run's brief wants its delta in.
    async fn list_events(&self, issue: &IssueId) -> Result<Vec<IssueEventRow>>;

    /// The timeline entries added to an issue after `since`, oldest first.
    /// This is the "what happened while you were away" a follow-up run's
    /// brief is built from, rather than replaying the whole history.
    async fn events_since(
        &self,
        issue: &IssueId,
        since: DateTime<Utc>,
    ) -> Result<Vec<IssueEventRow>>;

    /// Every run of one issue, newest first — the execution log.
    async fn list_runs(&self, issue: &IssueId) -> Result<Vec<IssueRunRow>>;

    /// The unfinished runs of one board. One query rather than a lookup per
    /// card: this is what tells the board which cards are working.
    async fn active_runs(&self, project: &ProjectId) -> Result<Vec<IssueRunRow>>;

    async fn get_run(&self, id: &IssueRunId) -> Result<Option<IssueRunRow>>;

    /// Every unfinished run, oldest first. The boot sweep's scan.
    async fn unsettled_runs(&self) -> Result<Vec<IssueRunRow>>;

    /// Claim a queued run: stamp it `running` with the session it will
    /// execute in. `Ok(false)` if it is no longer queued — which is how a
    /// double dispatch resolves into one execution.
    async fn claim_run(&self, id: &IssueRunId, session: &SessionId) -> Result<bool>;

    /// Settle a run. Terminal and idempotent: a replay of the same outcome
    /// is a no-op, so the boot re-drive can never double-settle.
    /// `Ok(false)` if it was already settled.
    async fn settle_run(
        &self,
        id: &IssueRunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<bool>;

    /// Return every unsettled run to `queued`, clearing the session it was
    /// claimed with. Run once at boot: a `running` row whose actor died
    /// with the process is work that never finished, not work in flight.
    async fn requeue_unsettled(&self) -> Result<Vec<IssueRunRow>>;
}

/// What caused a run to be enqueued. Shown verbatim in the execution log,
/// so the operator can tell "I dragged this" from "the comment I left woke
/// it" without opening the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTrigger {
    /// The issue entered In Progress — a drag, a REST move, an agent tool.
    Started,
    /// An agent was put on an issue already in In Progress.
    Assigned,
    /// A retry of a settled run.
    Retry,
}

impl RunTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            RunTrigger::Started => "started",
            RunTrigger::Assigned => "assigned",
            RunTrigger::Retry => "retry",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "started" => Some(RunTrigger::Started),
            "assigned" => Some(RunTrigger::Assigned),
            "retry" => Some(RunTrigger::Retry),
            _ => None,
        }
    }
}

/// Where a run is. `Queued` and `Running` are the unsettled states — the
/// ones the boot sweep re-drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Done => "done",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(RunStatus::Queued),
            "running" => Some(RunStatus::Running),
            "done" => Some(RunStatus::Done),
            "failed" => Some(RunStatus::Failed),
            "cancelled" => Some(RunStatus::Cancelled),
            _ => None,
        }
    }

    /// Whether this run is finished. Unsettled runs hold the per-issue
    /// dedupe slot and are what the boot sweep picks up.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            RunStatus::Done | RunStatus::Failed | RunStatus::Cancelled
        )
    }
}

/// One row of `issue_runs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRunRow {
    pub id: IssueRunId,
    pub issue_id: IssueId,
    pub project_id: ProjectId,
    /// The issue's human address, denormalised so the execution log and the
    /// boot sweep don't have to join.
    pub number: i64,
    pub agent_id: AgentProfileId,
    /// Minted when the run is claimed; `None` while it is still queued.
    pub session_id: Option<SessionId>,
    pub trigger: RunTrigger,
    pub status: RunStatus,
    /// 1 for an issue's first run, incrementing thereafter.
    pub attempt: i64,
    /// Why it failed, when it did.
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
}

/// A run to enqueue. Attempt and timestamps are the store's to assign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIssueRun {
    pub id: IssueRunId,
    pub issue_id: IssueId,
    pub project_id: ProjectId,
    pub number: i64,
    pub agent_id: AgentProfileId,
    pub trigger: RunTrigger,
}
