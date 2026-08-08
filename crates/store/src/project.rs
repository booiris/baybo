//! Persistence interface for kanban projects and the issues on their board.

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
    /// Most urgent first — the order a triage read wants, and the single
    /// source for every enum-valued schema and picker.
    pub const ALL: [IssuePriority; 5] = [
        IssuePriority::Urgent,
        IssuePriority::High,
        IssuePriority::Medium,
        IssuePriority::Low,
        IssuePriority::None,
    ];

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
    /// How much this project's agents may spend per UTC day. `None` is no
    /// ceiling — the default, because a board that stops working against a
    /// limit nobody chose is a board whose silence nobody can explain.
    pub daily_budget: Option<baybo_model::MicroUsd>,
    /// When the operator last looked at this board. `None` means never.
    pub read_at: Option<DateTime<Utc>>,
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
    /// Full-replace like the rest of this struct: `None` clears the ceiling.
    pub daily_budget: Option<baybo_model::MicroUsd>,
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
    /// The branch this issue's work landed on, once it has landed. `None`
    /// while the worktree exists but has produced no commit — which is the
    /// state a research issue stays in for its whole life, and is why the
    /// UI can key "show a branch" on this field alone rather than on a
    /// second has-commits flag that could disagree with it.
    pub branch: Option<String>,
    /// The issue this one is a sub-issue of. One level only: a child
    /// cannot itself be a parent, so a board is a list of cards and their
    /// steps rather than a tree nobody can read at a glance.
    pub parent_issue_id: Option<IssueId>,
    /// Which barrier this child belongs to under its parent. Stage `N`
    /// starts when every non-cancelled child of stage `N-1` is Done.
    /// Meaningless — and always `0` — on an issue with no parent.
    pub stage: i64,
    /// What opened this card. See [`NewIssue::source_key`].
    pub source_key: Option<String>,
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
    /// `Some(None)` detaches from the parent; `None` leaves it alone.
    pub parent: Option<Option<IssueId>>,
    pub stage: Option<i64>,
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
            && self.parent.is_none()
            && self.stage.is_none()
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
    /// The issue this one is a step of, if it is one.
    pub parent_issue_id: Option<IssueId>,
    pub stage: i64,
    /// What opened this card, for a caller that must not open it twice.
    pub source_key: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// The storage spelling of the operator, and of the board acting on its
/// own.
pub(crate) const ACTOR_USER: &str = "user";

/// See [`ACTOR_USER`].
pub(crate) const ACTOR_SYSTEM: &str = "system";

/// Who did the thing a timeline entry records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueActor {
    User,
    Agent(AgentProfileId),
    /// The board acting on its own — today, the budget gate.
    System,
}

impl IssueActor {
    /// Storage form. Agents are prefixed rather than stored bare so the
    /// cases stay distinguishable even if an agent is ever named `user`.
    pub fn to_storage(&self) -> String {
        match self {
            IssueActor::User => ACTOR_USER.to_owned(),
            IssueActor::System => ACTOR_SYSTEM.to_owned(),
            IssueActor::Agent(id) => format!("agent:{}", id.as_str()),
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            ACTOR_USER => Some(IssueActor::User),
            ACTOR_SYSTEM => Some(IssueActor::System),
            other => other
                .strip_prefix("agent:")
                .and_then(|id| AgentProfileId::parse(id.to_owned()).ok())
                .map(IssueActor::Agent),
        }
    }
}

/// What one timeline entry says.
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
    /// The issue's worktree was given back. `branch_deleted` says whether
    /// the branch went with it, which only happens when it never produced
    /// a commit.
    WorktreeReclaimed {
        branch_deleted: bool,
    },
    /// The worktree was left in place, and why. Almost always uncommitted
    /// work: the checkout holds the only copy, so the operator gets told
    /// rather than the board deciding for them.
    WorktreeKept {
        reason: String,
    },
    /// A run asked the operator to approve a tool call. Recorded before
    /// the prompt is answered, so the card shows work that is *waiting on
    /// a person* rather than work that has mysteriously stopped.
    ApprovalRequested {
        call_id: String,
        /// The tool whose call is blocked.
        tool: String,
        /// One line a person can decide from — the tool's own label when it
        /// has one, else a truncated parameter preview.
        summary: String,
    },
    /// …and what was decided. Also written when nobody decided: the gate
    /// denies on timeout, and a card that stops explaining itself at the
    /// prompt is the worst version of this feature.
    ApprovalResolved {
        call_id: String,
        decision: baybo_model::ApprovalDecision,
    },
    /// Every non-cancelled child in one of this issue's stages reached
    /// Done.
    StageCompleted {
        stage: i64,
    },
    /// The run was recorded but not started: this project has spent its
    /// budget for the day. The row is [`RunStatus::Held`] — unsettled, so
    /// the work is not lost and the issue's dedupe slot stays taken — and it
    /// starts as soon as the board has headroom again.
    BudgetExhausted {
        /// Micro-USD spent today, and the ceiling it reached.
        spent_micros: i64,
        limit_micros: i64,
    },
    /// A held run was released and started.
    BudgetRestored {
        spent_micros: i64,
        limit_micros: i64,
    },
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
            IssueEventBody::WorktreeReclaimed { .. } => "worktree_reclaimed",
            IssueEventBody::WorktreeKept { .. } => "worktree_kept",
            IssueEventBody::ApprovalRequested { .. } => "approval_requested",
            IssueEventBody::ApprovalResolved { .. } => "approval_resolved",
            IssueEventBody::StageCompleted { .. } => "stage_completed",
            IssueEventBody::BudgetExhausted { .. } => "budget_exhausted",
            IssueEventBody::BudgetRestored { .. } => "budget_restored",
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
    async fn list_projects(&self, include_archived: bool) -> Result<Vec<ProjectRow>>;

    async fn get_project(&self, id: &ProjectId) -> Result<Option<ProjectRow>>;

    async fn create_project(&self, row: &ProjectRow) -> Result<()>;

    async fn update_project(&self, id: &ProjectId, update: &ProjectUpdate) -> Result<bool>;

    async fn spend_since(
        &self,
        project: &ProjectId,
        since: DateTime<Utc>,
    ) -> Result<baybo_model::MicroUsd>;

    async fn attention(&self) -> Result<Vec<(ProjectId, AttentionCounts)>>;

    async fn projects_for_sessions(
        &self,
        sessions: &[SessionId],
    ) -> Result<Vec<(SessionId, ProjectId)>>;

    async fn project_feed(
        &self,
        project: &ProjectId,
        before: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Result<Vec<IssueEventRow>>;

    async fn live_issue_by_source_key(
        &self,
        project: &ProjectId,
        source_key: &str,
    ) -> Result<Option<IssueRow>>;

    async fn list_children(&self, parent: &IssueId) -> Result<Vec<IssueRow>>;

    async fn hold_run(&self, id: &IssueRunId) -> Result<bool>;

    async fn held_runs(&self, project: &ProjectId) -> Result<Vec<IssueRunRow>>;

    async fn release_run(&self, id: &IssueRunId) -> Result<bool>;

    async fn mark_project_read(&self, id: &ProjectId, at: DateTime<Utc>) -> Result<bool>;

    async fn set_project_archived(&self, id: &ProjectId, archived: bool) -> Result<bool>;

    async fn list_issues(&self, project: &ProjectId) -> Result<Vec<IssueRow>>;

    async fn get_issue(&self, project: &ProjectId, number: i64) -> Result<Option<IssueRow>>;

    async fn create_issue(&self, new: &NewIssue) -> Result<IssueRow>;

    async fn update_issue(
        &self,
        project: &ProjectId,
        number: i64,
        update: &IssueUpdate,
    ) -> Result<bool>;

    async fn move_issue(
        &self,
        project: &ProjectId,
        number: i64,
        status: IssueStatus,
        ordered_numbers: &[i64],
    ) -> Result<bool>;

    async fn enqueue_run(&self, new: &NewIssueRun) -> Result<IssueRunRow>;

    async fn append_event(&self, new: &NewIssueEvent) -> Result<IssueEventRow>;

    async fn list_events(&self, issue: &IssueId) -> Result<Vec<IssueEventRow>>;

    async fn events_since(
        &self,
        issue: &IssueId,
        since: DateTime<Utc>,
    ) -> Result<Vec<IssueEventRow>>;

    async fn set_issue_branch(&self, id: &IssueId, branch: &str) -> Result<bool>;

    async fn list_runs(&self, issue: &IssueId) -> Result<Vec<IssueRunRow>>;

    async fn active_runs(&self, project: &ProjectId) -> Result<Vec<IssueRunRow>>;

    async fn get_run(&self, id: &IssueRunId) -> Result<Option<IssueRunRow>>;

    async fn claim_run(&self, id: &IssueRunId, session: &SessionId) -> Result<bool>;

    async fn settle_run(
        &self,
        id: &IssueRunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<bool>;

    async fn requeue_unsettled(&self) -> Result<()>;
}

/// What caused a run to be enqueued. Shown verbatim in the execution log,
/// so the operator can tell "I dragged this" from "the comment I left woke
/// it" without opening the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTrigger {
    /// The issue entered In Progress — a drag, a REST move, an agent tool.
    Started,
    /// An agent was put on an issue already in In Progress, possibly
    /// replacing another.
    Assigned,
    /// A retry of a settled run.
    Retry,
    /// Somebody commented on live work that nobody was reading.
    Comment,
    /// Every child in one of this issue's stages finished **and nothing
    /// earlier was still open**, so its assignee was woken to drive what
    /// comes next. Strictly narrower than
    /// [`IssueEventBody::StageCompleted`], which says only that a stage
    /// emptied: a later stage emptying out of order is announced and wakes
    /// nobody.
    StageBarrier,
}

impl RunTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            RunTrigger::Started => "started",
            RunTrigger::Assigned => "assigned",
            RunTrigger::Retry => "retry",
            RunTrigger::Comment => "comment",
            RunTrigger::StageBarrier => "stage_barrier",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "started" => Some(RunTrigger::Started),
            "assigned" => Some(RunTrigger::Assigned),
            "retry" => Some(RunTrigger::Retry),
            "comment" => Some(RunTrigger::Comment),
            "stage_barrier" => Some(RunTrigger::StageBarrier),
            _ => None,
        }
    }
}

/// Where a run is. `Held`, `Queued` and `Running` are the unsettled states
/// — the ones that hold an issue's dedupe slot. `Running` and `Queued` are
/// both re-driven at a process start, in two steps:
/// [`ProjectStore::requeue_unsettled`] rolls every `running` row back to
/// `queued`, because its actor died with the process, and the caller then
/// hands out the `queued` rows board by board — a `queued` row was never
/// claimed by an executor, so neither state is work in flight. `Held` is
/// left alone — a hold was never started on purpose, so the budget decides
/// when it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Recorded but deliberately not started: the project has spent its
    /// budget for the day. A state, not an error — the row holds the
    /// issue's dedupe slot and starts the moment the board has headroom,
    /// so nothing is lost and nothing is silently dropped.
    Held,
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Held => "held",
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Done => "done",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "held" => Some(RunStatus::Held),
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

/// What is waiting on the operator on one board.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttentionCounts {
    /// Tool calls parked on an approval prompt. Filled from the channel's
    /// live queue rather than from the timeline — the queue cannot show a
    /// prompt that already timed out, and pairing request/resolution by
    /// `call_id` in SQL would scan a table that grows forever.
    pub approvals: usize,
    /// Runs recorded but not started, because the board is over budget.
    pub held: usize,
    /// Since the operator last looked: agents' comments and cards arriving
    /// in Review. Unlike the other three this is time-based, because
    /// reading either of them changes nothing a query could see.
    pub unread: usize,
    /// Live cards whose newest run failed. Nothing retries by itself, so
    /// these sit until somebody looks.
    pub failed: usize,
}

impl AttentionCounts {
    pub fn total(self) -> usize {
        self.approvals + self.held + self.failed + self.unread
    }

    pub fn is_empty(self) -> bool {
        self.total() == 0
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
    /// The session this run executes in — one per *agent* that works the
    /// card, not one per card, minted on that agent's first run and reused
    /// by its later ones. Stamped when the run is claimed, so `None` on a
    /// run no executor has picked up; a run the boot sweep returned to the
    /// queue keeps the session it was already working in.
    pub session_id: Option<SessionId>,
    pub trigger: RunTrigger,
    pub status: RunStatus,
    /// 1 for an issue's first run, incrementing thereafter.
    pub attempt: i64,
    /// Why it failed, when it did.
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    /// When an executor claimed it. **Not** "did this run start" — see
    /// [`Self::was_claimed`]: the boot sweep clears this when it returns an
    /// interrupted run to the queue, on the assumption that the next claim
    /// re-stamps it, so a run the sweep called off instead of re-driving
    /// reads as never started in the execution log even though it ran.
    pub started_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
}

impl IssueRunRow {
    /// Whether an executor ever picked this run up — which is the same
    /// question as "does the card already say this run started?".
    pub fn was_claimed(&self) -> bool {
        self.session_id.is_some()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_actor_round_trips_through_storage() {
        for actor in [
            IssueActor::User,
            IssueActor::System,
            IssueActor::Agent(AgentProfileId::parse("dev-1".to_owned()).expect("agent id")),
        ] {
            assert_eq!(
                IssueActor::parse(&actor.to_storage()),
                Some(actor.clone()),
                "{actor:?}"
            );
        }
    }
}
