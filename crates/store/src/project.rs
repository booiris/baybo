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

/// How many runs a board starts on its own before it waits for one to end.
///
/// The single home for the number: the `projects.max_parallel_issue_runs` column
/// is nullable with no SQL default, so a row that predates it resolves
/// here instead of at a second literal the DDL would have to keep in step.
pub const DEFAULT_MAX_PARALLEL_ISSUE_RUNS: usize = 3;

/// Whether a board's agents may land their own branches in the repository's
/// own checkout.
///
/// Off unless a board says otherwise, for the same reason the ceiling above
/// resolves here: the `projects.agents_may_merge` column is nullable with no
/// SQL default, so a row written before it existed answers here rather than
/// at a second literal in the DDL.
///
/// **Advisory in the `false` direction.** A run carries `Bash` and a
/// writable checkout, and `git merge` is not a destructive command, so a
/// board with this off can still be talked into merging by hand. What the
/// flag decides is whether the board *invites* it and whether `IssueMerge`
/// will do it — not whether git is reachable.
pub const DEFAULT_AGENTS_MAY_MERGE: bool = false;

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
    /// Daily token ceiling over the same rows as [`Self::daily_budget`]; `None` is unlimited.
    pub daily_budget_tokens: Option<i64>,
    /// How many runs this board may start **on its own** at once, by
    /// promoting Todo cards into In Progress. `0` turns the driver off and
    /// leaves promotion to whoever drags the card.
    ///
    /// Not `Option`: every board has an answer, and a row written before
    /// the column existed resolves to [`DEFAULT_MAX_PARALLEL_ISSUE_RUNS`] at the
    /// storage edge rather than making every reader decide again.
    pub max_parallel_issue_runs: usize,
    /// Whether this board's agents may merge a card's branch into the
    /// repository's own checkout, through `IssueMerge`.
    ///
    /// Not `Option`, for [`ProjectRow::max_parallel_issue_runs`]'s reason: a
    /// row written before the column existed resolves to
    /// [`DEFAULT_AGENTS_MAY_MERGE`] at the storage edge.
    pub agents_may_merge: bool,
    /// When an operator last changed a **rule** this board schedules by:
    /// either ceiling, [`Self::max_parallel_issue_runs`],
    /// [`Self::agents_may_merge`], or a restore from the archive.
    ///
    /// Scheduling state rather than content, and nothing draws it. The
    /// driver's `already_asked` reads it so that a standing question the
    /// lead has already answered becomes a question again when the premise
    /// of that answer changed: "escalate this to somebody who may merge" is
    /// the answer to a board that could not, and the board turning that on
    /// is the only thing that ever happens next.
    ///
    /// Not `Option`, for [`Self::max_parallel_issue_runs`]'s reason: a row
    /// written before the column existed resolves to its `created_at` at
    /// the storage edge. Every card is younger than its board, so a board
    /// whose rules never changed is exactly a board this re-opens nothing
    /// on.
    pub rules_changed_at: DateTime<Utc>,
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
    /// Full replacement; `None` clears the token ceiling.
    pub daily_budget_tokens: Option<i64>,
    /// See [`ProjectRow::max_parallel_issue_runs`]. `0` stops the board driving
    /// itself.
    pub max_parallel_issue_runs: usize,
    /// See [`ProjectRow::agents_may_merge`]. Full-replace like the rest of
    /// this struct, so a caller that omits it turns merging **off**.
    pub agents_may_merge: bool,
}

/// A file hung on a card — on its description, or on one comment.
///
/// Only `blob_id` and `filename` ever come from a client. `mime_type` and
/// `size` are read back off [`crate::BlobStore::stat`] at the write door, for
/// the reason [`baybo_model::ContentBlock`] gives for its own probed fields:
/// they are what the context budget spends when the file reaches a model, so
/// the uploader's word for them is not good enough.
///
/// There is no `kind` field. Which of image / audio / file this is falls out
/// of `mime_type` (`kind_of_mime`), so no stored discriminator can ever
/// disagree with the bytes it describes.
///
/// `Eq`, because [`IssueEventBody`] is — and that is why this is its own
/// small type rather than a [`baybo_model::ContentBlock`], which is only
/// `PartialEq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueAttachment {
    /// Capability id from the blob store: `sha256:<64hex>.<read token>`.
    pub blob_id: String,
    pub mime_type: String,
    pub size: u32,
    /// The name the file was uploaded under. `None` for a paste, which
    /// genuinely has none — not to spare a caller from passing one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
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
    /// Files hung on the description. Ordered as the uploader added them.
    pub attachments: Vec<IssueAttachment>,
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
    /// Kept in front of the operator: a pinned card is read first in its
    /// column, above even the cards carrying something new.
    ///
    /// A **reading** order and nothing else. It never touches `position`,
    /// and it is deliberately absent from `driver::promotion_order` — what
    /// the board works on next is `priority`, and two fields answering that
    /// question is one of them being wrong.
    pub pinned: bool,
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
    /// cannot itself be a parent, so the board's **hierarchy** is a list of
    /// cards and their steps rather than a tree nobody can read at a
    /// glance. [`Self::filed_from`] is the other card-to-card edge and is
    /// deliberately not this one: it records where a card came from, is
    /// unbounded in depth, and schedules nothing.
    pub parent_issue_id: Option<IssueId>,
    /// Which barrier this child belongs to under its parent. Stage `N`
    /// starts when every non-cancelled child of stage `N-1` is Done.
    /// Meaningless — and always `0` — on an issue with no parent.
    pub stage: i64,
    /// What opened this card. See [`NewIssue::source_key`].
    pub source_key: Option<String>,
    /// The card whose run filed this one. Provenance, not hierarchy.
    ///
    /// Named after the mechanism the board can actually vouch for. It knows
    /// which card's work was executing when the card was opened; it does
    /// not know what *caused* the finding, and a description routinely
    /// names an origin the filing card is not. The editorial claim stays in
    /// the prose, where it can name two origins and a reason; this holds
    /// the one edge that is a fact.
    ///
    /// **Written once, at creation, and never edited** — deliberately
    /// absent from [`IssueUpdate`]. Numbers are `MAX(number) + 1` per
    /// board, so a write-once edge to an already-existing card always
    /// points at a smaller number and the relation is acyclic by
    /// construction. Nothing here detects a cycle; making this patchable
    /// would be the change that first needs one.
    ///
    /// It schedules nothing: `parent_issue_id` arms the stage barrier and
    /// wakes the parent's assignee, and this wakes nobody.
    pub filed_from: Option<IssueId>,
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
    /// Full replace, like `description`: the client sends the list it wants
    /// to end up with, so removing the last attachment is `Some(vec![])` and
    /// not an absence that could be read as "leave them alone".
    ///
    /// Written by `ProjectManager::update_issue` from the blob ids it was
    /// handed, and by nothing else — resolving an id into one of these is
    /// what the manager is for. Setting it on a patch handed to the manager
    /// is overwritten.
    pub attachments: Option<Vec<IssueAttachment>>,
    pub priority: Option<IssuePriority>,
    /// `Some(None)` detaches from the parent; `None` leaves it alone.
    pub parent: Option<Option<IssueId>>,
    pub stage: Option<i64>,
    /// `Some(None)` unassigns; `None` leaves the assignee alone.
    pub assignee: Option<Option<AgentProfileId>>,
    pub blocked_reason: Option<Option<String>>,
    pub cancelled: Option<bool>,
    /// Singly optional, unlike its neighbours: there is no third state to
    /// express. A pin is on or off, and absent leaves it as it was.
    pub pinned: Option<bool>,
}

impl IssueUpdate {
    /// Whether this patch would change anything. A body that sets no field
    /// is a caller mistake worth a 400, not a silent no-op write.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.attachments.is_none()
            && self.priority.is_none()
            && self.parent.is_none()
            && self.stage.is_none()
            && self.assignee.is_none()
            && self.blocked_reason.is_none()
            && self.cancelled.is_none()
            && self.pinned.is_none()
    }
}

/// A new issue, before the store assigns its number and position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIssue {
    pub id: IssueId,
    pub project_id: ProjectId,
    pub title: String,
    pub description: String,
    pub attachments: Vec<IssueAttachment>,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    pub assignee: Option<AgentProfileId>,
    /// The issue this one is a step of, if it is one.
    pub parent_issue_id: Option<IssueId>,
    pub stage: i64,
    /// What opened this card, for a caller that must not open it twice.
    pub source_key: Option<String>,
    /// The card whose run filed this one. See [`IssueRow::filed_from`].
    pub filed_from: Option<IssueId>,
    pub created_at: DateTime<Utc>,
}

/// The storage spelling of the operator, and of the board acting on its
/// own.
pub(crate) const ACTOR_USER: &str = "user";

/// See [`ACTOR_USER`].
pub(crate) const ACTOR_SYSTEM: &str = "system";

/// What an agent's storage spelling starts with. Named because SQL that
/// asks "did an agent do this" has to spell it too, and a prefix with
/// three spellings is one of them being wrong.
pub const ACTOR_AGENT_PREFIX: &str = "agent:";

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
            IssueActor::Agent(id) => format!("{ACTOR_AGENT_PREFIX}{}", id.as_str()),
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            ACTOR_USER => Some(IssueActor::User),
            ACTOR_SYSTEM => Some(IssueActor::System),
            other => other
                .strip_prefix(ACTOR_AGENT_PREFIX)
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
        /// Files hung on this comment. `#[serde(default)]` is not
        /// decoration: `event_from_raw` turns any failed deserialize into a
        /// hard error and the timeline query collects into one `Result`, so
        /// a body written before this field existed would take out the whole
        /// card's timeline, brief and board feed — not just its own row.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<IssueAttachment>,
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
    /// A process restart found this run in flight and requeued it.
    RunInterrupted {
        run_id: IssueRunId,
        attempt: i64,
        resumes: i64,
    },
    RunSettled {
        run_id: IssueRunId,
        attempt: i64,
        status: RunStatus,
        error: Option<String>,
    },
    /// A door started a run and the card's live-run slot refused it.
    ///
    /// The board write that implied the run has already committed by the
    /// time the ledger says no — a drag is in the new column, a handover
    /// names the new agent, a stage says it opened — so without this entry
    /// the card records a change that nothing acted on, and the only trace
    /// is a log line the operator never sees. It is also the only thing in
    /// the tree that counts how often the dedupe guard refuses anything.
    ///
    /// Deliberately not a `Comment`: a `System`-actored comment satisfies
    /// `comments::somebody_asked_for_more`, so the settling run would wake
    /// its assignee on the board's own note.
    RunRefused {
        /// The run that was not started.
        trigger: RunTrigger,
        /// Which attempt is holding the slot, addressed the way the card
        /// addresses every other run. `None` when the row could not be read
        /// back — the refusal still happened and is still worth saying.
        attempt: Option<i64>,
    },
    Blocked {
        reason: String,
    },
    Unblocked,
    Cancelled,
    /// A cancel was taken back and the card is live work again.
    ///
    /// Recorded because [`crate::project::IssueActor`] on the two entries is
    /// what tells a person's stop from an agent's, and a reversal that
    /// wrote nothing left the card saying only that it had been called off.
    Uncancelled,
    /// The issue's worktree was given back. `branch_deleted` says whether
    /// the branch went with it, which only happens when it never produced
    /// a commit.
    WorktreeReclaimed {
        branch_deleted: bool,
    },
    /// A card's branch was merged into the repository's own checkout, by an
    /// agent that called `IssueMerge` on a board whose
    /// [`ProjectRow::agents_may_merge`] is on.
    ///
    /// `into` is stored rather than assumed: `git merge` lands on whatever
    /// branch the repository's checkout is on, so a board whose repo is
    /// parked somewhere other than its trunk merges *there*, and a card that
    /// did not say where is one nobody can audit afterwards.
    BranchMerged {
        branch: String,
        into: String,
        /// The merge commit. Empty only when git would not say, which is
        /// not worth failing the whole merge over.
        commit: String,
        /// How many commits the card contributed.
        commits: usize,
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
        /// Which run is parked, by its attempt number — the same address
        /// the execution log uses. Absent on entries written before the
        /// card started recording it, and on a prompt raised outside any
        /// run; the card says "a tool is waiting" either way, and inventing
        /// an attempt would be worse than not naming one.
        #[serde(default)]
        attempt: Option<i64>,
        /// The tool whose call is blocked.
        tool: String,
        /// One line a person can decide from — the tool's own label when it
        /// has one, else a truncated parameter preview.
        summary: String,
    },
    /// …and what was decided. Also written when nobody decided: the gate
    /// denies on timeout, the prompt dies with its run, and a card that
    /// stops explaining itself at the prompt is the worst version of this
    /// feature.
    ApprovalResolved {
        call_id: String,
        decision: baybo_model::ApprovalDecision,
        /// *How* it resolved — a decision, a timeout, an abandoned prompt.
        /// Defaulted so entries written before the distinction existed read
        /// as answered.
        #[serde(default)]
        resolution: baybo_model::ApprovalResolution,
    },
    /// Every non-cancelled child in one of this issue's stages reached
    /// Done.
    StageCompleted {
        stage: i64,
    },
    /// A run on this card opened another one, by its number. Recorded on
    /// the **origin**, which is the direction nothing else answers: the new
    /// card carries where it came from, and without this the card it came
    /// from falls silent at the moment its review spun out three more.
    Filed {
        number: i64,
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
    /// Token counterpart to [`Self::BudgetExhausted`], kept separate for wire-safe units.
    TokenBudgetExhausted {
        spent_tokens: i64,
        limit_tokens: i64,
    },
    /// A token-budget hold was released and started.
    TokenBudgetRestored {
        spent_tokens: i64,
        limit_tokens: i64,
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
            IssueEventBody::RunInterrupted { .. } => "run_interrupted",
            IssueEventBody::RunSettled { .. } => "run_settled",
            IssueEventBody::RunRefused { .. } => "run_refused",
            IssueEventBody::Blocked { .. } => "blocked",
            IssueEventBody::Unblocked => "unblocked",
            IssueEventBody::Cancelled => "cancelled",
            IssueEventBody::Uncancelled => "uncancelled",
            IssueEventBody::BranchMerged { .. } => "branch_merged",
            IssueEventBody::WorktreeReclaimed { .. } => "worktree_reclaimed",
            IssueEventBody::WorktreeKept { .. } => "worktree_kept",
            IssueEventBody::Filed { .. } => "filed",
            IssueEventBody::ApprovalRequested { .. } => "approval_requested",
            IssueEventBody::ApprovalResolved { .. } => "approval_resolved",
            IssueEventBody::StageCompleted { .. } => "stage_completed",
            IssueEventBody::BudgetExhausted { .. } => "budget_exhausted",
            IssueEventBody::BudgetRestored { .. } => "budget_restored",
            IssueEventBody::TokenBudgetExhausted { .. } => "token_budget_exhausted",
            IssueEventBody::TokenBudgetRestored { .. } => "token_budget_restored",
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

/// The two marks the "should the lead be told this board ran dry" question
/// is answered by.
///
/// Both are `None` on a board nothing has ever run on. Timestamps rather
/// than a `bool` for the reason every other guard in this crate is a
/// comparison: a stored flag is a second copy of the answer, free to
/// disagree with the rows it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrainMarks {
    /// When somebody last read this board and acted on it: any coordination
    /// run, not only a previous drain — and any run that was **cancelled**,
    /// because calling one off is a decision, and whoever took it had the
    /// board in front of them.
    ///
    /// Deliberately wider than the question it guards. A coordination brief
    /// tells the lead to read the whole board, so a lead woken since the
    /// last work settled has already been shown everything a drain question
    /// could show it, and telling it again inside the same lull buys a
    /// billed run and no new information. An ask the dispatcher killed
    /// before it was claimed never reached the lead and does not count, on
    /// the same rule `driver::already_asked` applies to a dead ask.
    pub looked_at: Option<DateTime<Utc>>,
    /// When a **work** run on this board last settled. Coordination is
    /// excluded on both sides: the lead being asked a question, and
    /// answering it, is not the board doing work. So is a run that was
    /// cancelled — see `looked_at`, which is the side it lands on.
    pub worked_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait ProjectStore: Send + Sync {
    async fn list_projects(&self, include_archived: bool) -> Result<Vec<ProjectRow>>;

    async fn get_project(&self, id: &ProjectId) -> Result<Option<ProjectRow>>;

    async fn create_project(&self, row: &ProjectRow) -> Result<()>;

    async fn update_project(&self, id: &ProjectId, update: &ProjectUpdate) -> Result<bool>;

    /// Return money and token spend from the same rows since `since`.
    ///
    /// The board answers for every session its work happens in, which is
    /// wider than its runs: a subagent spawned by a run, and a cron fire
    /// that files onto the board without being anybody's run, both spend on
    /// the board's behalf. Wider than [`Self::run_spend`] by construction —
    /// a card whose total exceeded its board's would break the pairing the
    /// budget gate rests on.
    async fn spend_since(&self, project: &ProjectId, since: DateTime<Utc>) -> Result<Spend>;

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

    /// Stop a queued run on the budget, answering with the row **as it now
    /// stands** — `None` when the compare-and-set found it in some other
    /// state and wrote nothing.
    ///
    /// The row and not a `bool`, because the caller has the pre-write copy
    /// in its hand and a `bool` invites it to patch that copy into what it
    /// believes this wrote. It did, and got it wrong: the follow-up log line
    /// called a held run "queued", and `retry` answered 201 with a body
    /// saying the run was on its way. Whoever holds the ingredients cooks.
    async fn hold_run(&self, id: &IssueRunId) -> Result<Option<IssueRunRow>>;

    async fn held_runs(&self, project: &ProjectId) -> Result<Vec<IssueRunRow>>;

    /// Put a held run back in the queue. Answers like [`Self::hold_run`] and
    /// for the same reason: the released row is what gets dispatched, and
    /// the dispatcher's copy is documented as reading `Queued`.
    async fn release_run(&self, id: &IssueRunId) -> Result<Option<IssueRunRow>>;

    /// Note that the operator has opened this card. Monotonic: a slow
    /// request must not rewind the cursor and resurrect a badge that was
    /// already cleared.
    async fn mark_issue_read(&self, issue: &IssueId, at: DateTime<Utc>) -> Result<bool>;

    /// The same stamp on every card of one board, for an operator who has
    /// read the board rather than a card. Still one cursor per card —
    /// nothing here makes `read_at` mean anything new — and monotonic for
    /// the same reason. Every card is stamped, cancelled and finished ones
    /// included: the cursor says "seen", and a card being over is not a
    /// reason to go on counting what was said on it. Answers with the rows
    /// it moved, which is what makes the monotonic guard observable.
    async fn mark_project_read(&self, project: &ProjectId, at: DateTime<Utc>) -> Result<usize>;

    /// Every card on this board that has something waiting on it. Cards
    /// with nothing waiting are absent rather than present with zeroes.
    async fn card_signals(
        &self,
        project: &ProjectId,
    ) -> Result<std::collections::HashMap<IssueId, CardSignals>>;

    /// The cards on this board an **agent** opened, by number.
    ///
    /// Card authorship is recorded nowhere but the timeline, and this is
    /// one query rather than a timeline read per card on purpose: the board
    /// driver asks it every pass, and a Backlog full of the operator's own
    /// cards would otherwise cost a full event list each, every tick,
    /// forever.
    async fn agent_opened_issues(&self, project: &ProjectId) -> Result<Vec<i64>>;

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

    /// What each of an issue's runs spent. Derived from `cost_records`
    /// rather than stored on the run: a session is shared by every run the
    /// same agent does on the card, so the only thing that attributes a
    /// call to one run is the run's own window. That window is unambiguous
    /// because the enqueue dedupe guard keeps at most one run per issue in
    /// flight, so two windows on one session can never overlap. A run
    /// nobody claimed has no window and reads zero.
    ///
    /// The window covers the run's whole spawn tree, not just the session it
    /// works in directly: a subagent bills against its own session, and a
    /// run charged only for its own would let delegated work read as free.
    async fn run_spend(&self, issue: &IssueId) -> Result<Vec<RunSpend>>;

    /// The same derivation as [`Self::run_spend`], addressed by run rather
    /// than by card and carrying the run's duration with it.
    ///
    /// Batched because its caller is the board-wide feed: a page of it can
    /// name runs on a dozen different cards, and asking per card would turn
    /// one screen into a dozen round trips. Runs that do not exist are
    /// simply absent from the result.
    async fn settled_run_facts(&self, runs: &[IssueRunId]) -> Result<Vec<SettledRunFacts>>;

    /// Every board's live working count and spend since `since`, in one
    /// pass. One query rather than one per board: the switcher's dropdown
    /// asks about all of them at once, and a per-row read is the shape
    /// that turns a five-board dropdown into eleven round trips.
    async fn board_activity(&self, since: DateTime<Utc>)
    -> Result<Vec<(ProjectId, BoardActivity)>>;

    async fn active_runs(&self, project: &ProjectId) -> Result<Vec<IssueRunRow>>;

    /// The marks behind the board-scale "is there anything to say" guard.
    ///
    /// Board-scoped by necessity: [`RunTrigger::BoardIdle`] is a question
    /// about the whole board, so the run that asked it last may be filed
    /// against a different card each time and no card's own run list can
    /// answer whether the lead has seen the board since.
    async fn drain_marks(&self, project: &ProjectId) -> Result<DrainMarks>;

    async fn get_run(&self, id: &IssueRunId) -> Result<Option<IssueRunRow>>;

    async fn claim_run(&self, id: &IssueRunId, session: &SessionId) -> Result<bool>;

    async fn settle_run(
        &self,
        id: &IssueRunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<bool>;

    /// Atomically requeue in-flight runs and return their incremented resume counts.
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
    /// An agent was put on an issue already in In Progress, possibly
    /// replacing another.
    Assigned,
    /// A retry of a settled run.
    Retry,
    /// Somebody commented on live work that nobody was reading.
    Comment,
    /// The board took this card off the top of Todo by itself, because it
    /// had room. The counterpart of [`RunTrigger::Started`], and separate
    /// from it so the execution log can say "nobody asked for this — the
    /// board had capacity" without the operator opening the transcript.
    Promoted,
    /// The lead was woken to staff a card that reached Todo with nobody on
    /// it. The one run that happens on a card its runner is not assigned
    /// to: the lead is being asked *who should do this*, not to do it.
    Triage,
    /// Every child in one of this issue's stages finished **and nothing
    /// earlier was still open**, so its assignee was woken to drive what
    /// comes next. Strictly narrower than
    /// [`IssueEventBody::StageCompleted`], which says only that a stage
    /// emptied: a later stage emptying out of order is announced and wakes
    /// nobody.
    StageBarrier,
    /// The lead was woken because this card sits in Review with nobody
    /// working it — arranging the review is the lead's to do. Like
    /// [`RunTrigger::Triage`], the runner is not the card's assignee.
    Review,
    /// The lead was woken because this card sits in In Progress with no run
    /// working it and nothing queued — work that has silently stopped.
    /// Like [`RunTrigger::Triage`], the runner is not the card's assignee.
    Stalled,
    /// Lead coordination triggered by a blocked card.
    Blocked,
    /// The lead was woken because an **agent** parked this card in Backlog,
    /// a column the board never pulls from. Like [`RunTrigger::Triage`],
    /// the runner is not the card's assignee; unlike it, the question is
    /// asked only about the board's own work breakdown. A card the operator
    /// filed into Backlog is a decision to leave it alone, and the board
    /// does not reopen it — the same rule that keeps a person's block from
    /// waking anybody.
    Grooming,
    /// The lead was woken because the **board** has run dry: nothing
    /// executing, nothing queued, room to start something, and cards the
    /// board may take up still on it.
    ///
    /// The only question here that is about the board rather than about the
    /// card its run is filed against — that card is an anchor, because a run
    /// is a row on a card and this question has no card of its own. It is
    /// asked last and only when every per-card question declined, so it is
    /// by construction the board saying "I have looked at all of it and I
    /// have no move left".
    ///
    /// Carries [`RunTrigger::Grooming`]'s rule and not only its own, because
    /// this question hands the lead the whole board rather than one card: a
    /// Backlog card the **operator** parked is neither live work this counts
    /// nor a card it may anchor on. Asking it here would put the operator's
    /// own decision back in front of a lead told to find something to start.
    BoardIdle,
}

impl RunTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            RunTrigger::Started => "started",
            RunTrigger::Assigned => "assigned",
            RunTrigger::Retry => "retry",
            RunTrigger::Comment => "comment",
            RunTrigger::Promoted => "promoted",
            RunTrigger::Triage => "triage",
            RunTrigger::StageBarrier => "stage_barrier",
            RunTrigger::Review => "review",
            RunTrigger::Stalled => "stalled",
            RunTrigger::Blocked => "blocked",
            RunTrigger::Grooming => "grooming",
            RunTrigger::BoardIdle => "board_idle",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "started" => Some(RunTrigger::Started),
            "assigned" => Some(RunTrigger::Assigned),
            "retry" => Some(RunTrigger::Retry),
            "comment" => Some(RunTrigger::Comment),
            "promoted" => Some(RunTrigger::Promoted),
            "triage" => Some(RunTrigger::Triage),
            "stage_barrier" => Some(RunTrigger::StageBarrier),
            "review" => Some(RunTrigger::Review),
            "stalled" => Some(RunTrigger::Stalled),
            "blocked" => Some(RunTrigger::Blocked),
            "grooming" => Some(RunTrigger::Grooming),
            "board_idle" => Some(RunTrigger::BoardIdle),
            _ => None,
        }
    }

    /// Whether this is lead coordination rather than the card's own work.
    pub fn is_coordination(self) -> bool {
        matches!(
            self,
            RunTrigger::Triage
                | RunTrigger::Review
                | RunTrigger::Stalled
                | RunTrigger::Blocked
                | RunTrigger::Grooming
                | RunTrigger::BoardIdle
        )
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
///
/// Every one of these is an **event** — something arrived, or broke, or is
/// being asked. That is the whole membership rule, and it is why runs the
/// daily ceiling is holding are deliberately **not** here despite being the
/// most literal "only you can fix this" the board has.
///
/// A hold is a *standing condition*, not news: it does not arrive, it does
/// not stop being true until the operator changes a number, and the mark it
/// produced was therefore indistinguishable from one that could not be
/// cleared at all — which is exactly how it was reported. The board says it
/// where it can be acted on instead: an over-ceiling notice in the board's
/// own header, beside the settings that lift it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttentionCounts {
    /// Tool calls parked on an approval prompt. Filled from the channel's
    /// live queue rather than from the timeline — the queue cannot show a
    /// prompt that already timed out, and pairing request/resolution by
    /// `call_id` in SQL would scan a table that grows forever.
    pub approvals: usize,
    /// The sum of every card's [`CardSignals::unread`] on this board.
    /// Unlike the other two this is time-based, because reading an
    /// agent's comment changes nothing a query could otherwise see.
    pub unread: usize,
    /// Live cards whose newest run failed. Nothing retries by itself, so
    /// these sit until somebody looks.
    pub failed: usize,
}

/// What one card carries beyond its own row: the two signals derived over
/// its events and its runs.
///
/// Both live here rather than being re-derived per caller, because the
/// board's `attention` counts and the card's own badge have to be two views
/// of one predicate — a rail dot saying "3 failed" over a board on which no
/// card admits to failing is the exact drift this type exists to prevent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CardSignals {
    /// Events on this card the operator has not opened it since: an
    /// agent's comment, or an agent moving it into Review. The operator's
    /// own actions never count — their own words are not news to them.
    pub unread: usize,
    /// This card's newest run failed and the card is still live. Cleared
    /// by retrying, by finishing, by cancelling, or by blocking it —
    /// never by looking, which is what separates it from `unread`.
    pub last_run_failed: bool,
}

/// One board's cards with every card's signals already resolved.
///
/// The rows and the signals travel together because a caller holding only
/// the rows would have to ask what "this card's run failed" means, and that
/// question has exactly one home: [`ProjectStore::card_signals`].
#[derive(Debug, Clone, Default)]
pub struct BoardCards {
    pub rows: Vec<IssueRow>,
    signals: std::collections::HashMap<IssueId, CardSignals>,
    /// By number rather than by id, because that is how the timeline
    /// denormalises a card and one board's numbers are unique. Kept apart
    /// from [`CardSignals`], whose map is sparse on purpose — absent there
    /// means "nothing waiting", and authorship is true of most cards.
    opened_by_agent: std::collections::BTreeSet<i64>,
}

impl BoardCards {
    pub fn new(
        rows: Vec<IssueRow>,
        signals: std::collections::HashMap<IssueId, CardSignals>,
        opened_by_agent: std::collections::BTreeSet<i64>,
    ) -> Self {
        Self {
            rows,
            signals,
            opened_by_agent,
        }
    }

    /// A card with nothing waiting on it has no row in the map, rather
    /// than a row of zeroes — so absent has to read as quiet, not as
    /// missing.
    pub fn signals(&self, issue: &IssueId) -> CardSignals {
        self.signals.get(issue).copied().unwrap_or_default()
    }

    /// Whether an agent opened this card, rather than the operator.
    ///
    /// The same fact `RunTrigger::Grooming` is decided by, resolved here
    /// rather than left to each caller: a card face that answered it a
    /// second time could mark a card the board will never ask about.
    /// A card with no `Opened` entry reads as the operator's, which is the
    /// direction that leaves work parked.
    pub fn opened_by_agent(&self, number: i64) -> bool {
        self.opened_by_agent.contains(&number)
    }
}

/// What a board is doing right now, for the switcher's dropdown. Two
/// numbers rather than the whole board, because the dropdown's job is to
/// let the operator pick between boards without opening either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoardActivity {
    /// Runs actually executing. Counted from runs rather than from the
    /// In Progress column, because a run outlives its column — dragging a
    /// card out never kills it.
    pub working: usize,
    /// Spend since the caller's day began, using the budget gate's rows.
    pub burn: Spend,
}

impl AttentionCounts {
    pub fn total(self) -> usize {
        self.approvals + self.failed + self.unread
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
    /// Number of boot recoveries that requeued this run.
    pub resumes: i64,
    /// Why it failed, when it did.
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    /// First claim time, retained across requeues so the spend window stays complete.
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

/// What some LLM calls cost, summed. Tokens ride along with the money
/// because the rail shows the two together and a second round trip to the
/// same rows would only invite them to disagree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Spend {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost: baybo_model::MicroUsd,
}

impl Spend {
    /// Total prompt and completion tokens; cached-token columns are input subsets.
    pub fn tokens(self) -> i64 {
        self.input_tokens + self.output_tokens
    }
}

impl std::ops::Add for Spend {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cost: baybo_model::MicroUsd::from_micros(
                self.cost.into_micros() + other.cost.into_micros(),
            ),
        }
    }
}

impl std::iter::Sum for Spend {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), |acc, one| acc + one)
    }
}

/// One run's share of its session's spend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpend {
    pub run_id: IssueRunId,
    pub spend: Spend,
}

/// What one finished run took and cost.
///
/// Derived, never stored — same reason as [`ProjectStore::run_spend`], and
/// the same window. Freezing either number onto the run row or into its
/// timeline entry would put a third copy beside the ledger, and the copy
/// would be written before the run's last cost record necessarily is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledRunFacts {
    pub run_id: IssueRunId,
    /// `None` on a run no executor ever claimed, which has no window and so
    /// no duration — not zero, which would read as "instant".
    pub duration_ms: Option<i64>,
    pub spend: Spend,
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

    /// `is_empty` is the 400 gate on the REST patch *and* the branch the
    /// agent tool picks between reading a card and writing it — a field
    /// missing from the chain is a patch that is silently dropped rather
    /// than refused.
    #[test]
    fn every_field_of_a_patch_counts_as_setting_one() {
        assert!(IssueUpdate::default().is_empty());
        let each: [IssueUpdate; 10] = [
            IssueUpdate {
                title: Some("t".into()),
                ..Default::default()
            },
            IssueUpdate {
                description: Some("d".into()),
                ..Default::default()
            },
            IssueUpdate {
                attachments: Some(Vec::new()),
                ..Default::default()
            },
            IssueUpdate {
                priority: Some(IssuePriority::Urgent),
                ..Default::default()
            },
            IssueUpdate {
                parent: Some(None),
                ..Default::default()
            },
            IssueUpdate {
                stage: Some(1),
                ..Default::default()
            },
            IssueUpdate {
                assignee: Some(None),
                ..Default::default()
            },
            IssueUpdate {
                blocked_reason: Some(None),
                ..Default::default()
            },
            IssueUpdate {
                cancelled: Some(true),
                ..Default::default()
            },
            IssueUpdate {
                pinned: Some(true),
                ..Default::default()
            },
        ];
        for update in each {
            assert!(!update.is_empty(), "{update:?}");
        }
    }
}
