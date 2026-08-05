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
use baybo_model::{AgentProfileId, IssueId, ProjectId};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Which column an issue sits in. The set is fixed: entering
/// [`IssueStatus::InProgress`] is the single execution trigger, so a
/// user-definable column would be a user-definable trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
}
