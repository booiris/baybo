//! Project and issue lifecycle: validation, workdir materialisation, and
//! the board's write surface.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use baybo_model::{
    AgentFramework, AgentHandle, AgentProfileId, IssueEventId, IssueId, IssueRunId,
    MAX_PROJECT_NAME_CHARS, ProjectId, SessionId, TeamMembership,
};
use baybo_store::project::{
    BoardCards, DEFAULT_MAX_PARALLEL_ISSUE_RUNS, IssueActor, IssueEventAppendOutcome,
    IssueEventBody, IssueEventClientMsgId, IssueEventRow, IssuePriority, IssueRow, IssueRunRow,
    IssueStatus, IssueUpdate, NewIssue, NewIssueEvent, ProjectRow, ProjectStore, ProjectUpdate,
    RunStatus, RunTrigger, Spend,
};
use baybo_store::{AgentProfileStore, BlobStore};
use baybo_workspace::WorkspacePaths;
use chrono::{DateTime, Utc};

use crate::CommentDelivery;
use crate::attachments::{self, AttachmentRequest};
use crate::budget::Headroom;
use crate::error::{ProjectError, Result};
use crate::events::ProjectEvents;
use crate::runs::{RunOutcome, Transition, ledger_entry, triggers_run};

/// The columns a card's branch may be merged from.
///
/// Review is the point of it: the board keeps its reviewer, and the flag
/// only removes the person who used to run the merge afterwards. Done is
/// here too because a card can reach it before anybody asks for the merge,
/// and refusing then would strand exactly the work this exists to land.
///
/// Done cannot be the *only* one, and that is mechanical rather than a
/// preference: a Done card can host no run at all. A comment on one is
/// `RecordOnly`, no status change into Done triggers a run, `accepts_runs`
/// refuses the retry button, and the driver only looks at Todo. So a merge
/// that required Done could only ever happen inside the turn that closed the
/// card, and a turn that missed it would leave the branch unlandable by any
/// agent — which is the failure this whole verb exists to stop.
///
/// **This is a weak gate, knowingly.** Review says the author called the
/// work ready, not that anybody accepted it — the author moves its own card
/// there — and nothing here compares actors, so one agent can move its card
/// to Review and land it in the same turn. Nothing stores an approval to
/// check instead. See "What is still not built" in `docs/todo/kanban.md`
/// for what a real gate would test and why it is deferred.
const MERGEABLE_FROM: &[IssueStatus] = &[IssueStatus::Review, IssueStatus::Done];

/// Upper bound on an issue title (chars, after trim). Long enough for a
/// sentence, short enough that a card face can show it.
pub(crate) const MAX_ISSUE_TITLE_CHARS: usize = 200;

/// The handle every project's coordinator answers to. Fixed rather than
/// derived: `@lead` means the same thing on every board, and it is the one
/// handle a person can type without looking the team up first.
pub const LEAD_HANDLE: &str = "lead";

/// One line in a board's activity feed. Most lines are a card's timeline
/// entry; a hire belongs to the board itself and is derived from the roster
/// rather than written to any card's timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEntry {
    Card {
        row: IssueEventRow,
        /// What the run took and cost, on the entries that settled one.
        ///
        /// Attached here rather than left to the caller: it is derived over
        /// the run's own cost window, and a caller holding the store would
        /// re-derive it — differently, the second time. `None` on every
        /// other kind of entry, and on a settled run whose row is gone.
        settled: Option<SettledRun>,
    },
    Hired {
        agent: AgentProfileId,
        /// Absent when a person did the hiring — the lead's own arrival
        /// reads this way too, since nobody hired it: it comes with the
        /// board.
        hired_by: Option<AgentProfileId>,
        at: DateTime<Utc>,
    },
}

/// What a settled run took and cost, as a feed line says it.
///
/// The two resolved numbers, not the store's whole `SettledRunFacts`: the
/// feed does not show token counts and has no use for the run id it already
/// has, and a port that hands over the record instead of the answer invites
/// its caller to compute a different one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledRun {
    /// `None` on a run no executor claimed — not zero, which reads as a run
    /// that finished instantly.
    pub duration_ms: Option<i64>,
    pub cost_micros: i64,
}

impl FeedEntry {
    /// When it happened — the one thing both kinds are ordered by.
    pub fn at(&self) -> DateTime<Utc> {
        match self {
            Self::Card { row, .. } => row.created_at,
            Self::Hired { at, .. } => *at,
        }
    }
}

/// One run and what it cost, as the execution log shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWithSpend {
    pub run: IssueRunRow,
    pub spend: Spend,
}

/// A card's whole execution history, priced. `total` is over *every* run,
/// including the ones the log lists as cancelled — the money was spent
/// either way, and a total that quietly skipped them would not reconcile
/// against the board's budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRunLog {
    pub runs: Vec<RunWithSpend>,
    pub total: Spend,
}

const LEAD_DESCRIPTION: &str =
    "Coordinates this project's board: triages Backlog, assigns work, and staffs the team.";

/// How many live agents one project may have, lead included.
pub const MAX_TEAM_AGENTS: usize = 16;

/// How often the board driver looks at every live board.
///
/// The upper bound on how long a ready card sits in Todo after the thing
/// that made it ready — a drag, a run settling, the lead assigning it. Short
/// enough that a person watching the board sees it move; a pass is three
/// queries per board against local sqlite, so the tick is cheaper than the
/// UI it feeds.
const DRIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Upper bound on a teammate's role line (chars, after trim). It seeds a
/// SOUL and shows on a roster card, so it is a sentence, not a brief.
pub(crate) const MAX_ROLE_CHARS: usize = 280;

/// Upper bound on one activity-feed page.
pub const MAX_FEED_PAGE: usize = 100;

const RUN_CALLED_OFF_UNSTARTED: &str = "the card was finished or cancelled before this run started";

const RUN_CALLED_OFF_INTERRUPTED: &str =
    "this run was interrupted, and the card was finished or cancelled before it could resume";

const RUN_CANCELLED_BEFORE_STARTING: &str = "cancelled before it started";

const RUN_STOOD_DOWN_FOR_A_BLOCK: &str = r#"the card was blocked with a question, and this run stood down so the lead could be asked to answer it — the work starts again from the answer"#;

const RETRY_ON_A_BLOCKED_CARD: &str =
    "this issue is blocked — lift the block before running it again";

const AGENT_REOPENING_A_PERSONS_STOP: &str = "the operator cancelled this issue, so reopening it is theirs to do — say on the card why \
     you think it should come back, and leave it cancelled";

fn held_run_refusal(figures: Option<crate::budget::Figures>) -> String {
    let ceiling = match figures {
        Some(crate::budget::Figures::Tokens { .. }) => "daily token budget",
        _ => "daily budget",
    };
    format!(
        "this run is held — the project is over its {ceiling}, and starts as soon as there is room"
    )
}

fn framework_refusal(agent: &AgentProfileId, framework: AgentFramework) -> String {
    format!(
        "{agent} runs on {}, which cannot yet host an issue's session — assign a baybo agent",
        framework.as_str()
    )
}

fn call_off_reason(run: &IssueRunRow) -> &'static str {
    if run.was_claimed() {
        RUN_CALLED_OFF_INTERRUPTED
    } else {
        RUN_CALLED_OFF_UNSTARTED
    }
}

const MAX_HANDLE_ATTEMPTS: usize = 9;

/// A board's capacity, as [`ProjectManager::board_load`] reports it.
#[derive(Debug, Clone)]
pub(crate) struct BoardLoad {
    pub headroom: Headroom,
    /// Runs actually executing or queued to. What "who is free" means.
    pub working: Vec<IssueRunRow>,
    /// Runs recorded and deliberately not started, because the board is
    /// over budget. Idle work, not busy agents.
    pub held: Vec<IssueRunRow>,
}

/// What a caller supplies to put somebody new on a team.
#[derive(Debug, Clone)]
pub struct NewTeamMember {
    /// Display name, chosen once: the `@handle` is derived from it here and
    /// both are then fixed, so the roster and every mention keep agreeing.
    pub name: String,
    /// One line saying what this agent is for. Seeds its `SOUL.md` and
    /// becomes its roster description.
    pub role: String,
    /// `None` follows the workspace default (baybo). Only the operator's
    /// form sets this; `ProjectAgentCreate` deliberately does not.
    pub framework: Option<AgentFramework>,
    /// What this teammate runs on. [`LlmPin::unpinned`] follows the
    /// deployment's defaults at every level.
    pub llm: baybo_model::LlmPin,
}

/// What a caller supplies to open a project.
#[derive(Debug, Clone, Default)]
pub struct NewProject {
    pub name: String,
    pub description: String,
    /// Absolute path to an existing git repository. `None` means "make me
    /// one": the manager materialises `work/<slug>` and initialises it, so
    /// starting a project never requires having a repo first.
    pub workdir: Option<String>,
    /// Daily spend ceiling. `None` is no ceiling — a board should work out
    /// of the box, and a limit the operator did not choose is a board that
    /// stops for a reason nobody can explain.
    pub daily_budget: Option<baybo_model::MicroUsd>,
    /// Daily token ceiling. `None` is no ceiling.
    pub daily_budget_tokens: Option<i64>,
    /// How many runs the board may start on its own. `None` takes
    /// [`DEFAULT_MAX_PARALLEL_ISSUE_RUNS`]; `Some(0)` opens a board that only
    /// ever runs what somebody drags.
    pub max_parallel_issue_runs: Option<usize>,
    /// Whether the board's agents may merge a card's branch into the
    /// repository's own checkout. Off by default — a board that lands code
    /// on its trunk unattended is a decision, not a default.
    pub agents_may_merge: bool,
}

/// Where a card sits under a parent, as a wire caller expresses it: a
/// parent by **number**, the detach as its own intent, and the stage that
/// only means anything alongside one of them.
///
/// Resolved by [`ProjectManager::resolve_placement`], which is the one home
/// for reading it — the tool and the admin API both address a parent by
/// number, and two spellings of "what does `0` mean here" is one of them
/// being wrong.
#[derive(Debug, Clone, Default)]
pub struct Placement {
    /// The parent's number. Values below `1` are not issue numbers and are
    /// read as "leave the placement alone", never as a detach.
    pub parent: Option<i64>,
    /// Take the card out of its parent's plan, resetting its stage.
    pub detach: bool,
    /// Which barrier under the parent. Landed only with a `parent` or a
    /// `detach`, because half a placement is not a placement.
    pub stage: Option<i64>,
}

/// What one drive pass knows about the board it is looking at, past the
/// rows themselves.
struct BoardPass {
    /// Cards with a run recorded against them that is actually executing —
    /// block-parked rows hold a card's slot without spending capacity, so
    /// they are not in here.
    in_flight: std::collections::BTreeSet<i64>,
    /// When the operator last changed a rule this board schedules by. Read
    /// only by the board question — see
    /// `driver::nothing_has_happened_since_the_lead_looked`.
    rules_changed_at: chrono::DateTime<chrono::Utc>,
    promoted: usize,
}

/// What a caller supplies to open an issue. Status is where the card
/// lands; there is no separate "column" concept.
#[derive(Debug, Clone)]
pub struct NewIssueRequest {
    pub title: String,
    pub description: String,
    /// Files to hang on the description, by blob id. Resolved against the
    /// blob store before the row is written, so a card is never opened
    /// pointing at bytes that are not there.
    pub attachments: Vec<AttachmentRequest>,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    pub assignee: Option<AgentProfileId>,
    /// The issue this one is a step of, by its number on this board.
    pub parent: Option<i64>,
    /// Which barrier under that parent. Ignored without a parent.
    pub stage: i64,
    /// What is opening this card, for a caller that must not open it
    /// twice. See [`Opened`].
    pub source_key: Option<String>,
    /// The card whose run is filing this one, when one is. Derived from the
    /// calling session by the board tool — never taken from a model, never
    /// taken from a request body — and written once. See
    /// [`baybo_store::project::IssueRow::filed_from`].
    pub filed_from: Option<IssueId>,
}

/// What happened when a card was opened.
#[derive(Debug, Clone)]
pub enum Opened {
    Created(IssueRow),
    /// A live card already exists under this key. Nothing was written: no
    /// row, no timeline entry, no run. A daily check must not lay 365
    /// "already open" notes on one card.
    AlreadyOpen(IssueRow),
}

impl Opened {
    /// The card, however it was arrived at.
    pub fn issue(&self) -> &IssueRow {
        match self {
            Opened::Created(issue) | Opened::AlreadyOpen(issue) => issue,
        }
    }

    pub fn into_issue(self) -> IssueRow {
        match self {
            Opened::Created(issue) | Opened::AlreadyOpen(issue) => issue,
        }
    }

    pub fn was_created(&self) -> bool {
        matches!(self, Opened::Created(_))
    }
}

pub struct ProjectManager {
    store: Arc<dyn ProjectStore>,
    agents: Arc<dyn AgentProfileStore>,
    /// Where a card's files live. Held so that "is this a valid
    /// attachment" is answered *here*, on the path both the operator's
    /// REST write and an agent's tool call already pass through — rather
    /// than at each door, which is how two doors get two answers.
    blobs: Arc<dyn BlobStore>,
    paths: WorkspacePaths,
    /// Where a board change is announced to whoever is watching it.
    events: Arc<dyn ProjectEvents>,
    /// Where a recorded run is announced. The ledger row is written first
    /// and this only nudges — a send that fails, or a receiver that died,
    /// costs a delay until the next boot sweep, never a lost run.
    dispatch: RunDispatch,
    /// Serialises [`drive`](Self::drive). See the comment there: deciding
    /// how much room a board has and then filling it is a read-then-write,
    /// and every run that settles asks the question again.
    driving: tokio::sync::Mutex<()>,
    /// Boards where lead seeding failed.
    leadless: parking_lot::Mutex<std::collections::HashSet<ProjectId>>,
    /// Serialises [`merge_issue_branch`](Self::merge_issue_branch). Nothing
    /// else locks the repository, and a board runs several cards at once, so
    /// two merges would otherwise race `.git/index.lock` and one would come
    /// back reading like a conflict it never had.
    merging: tokio::sync::Mutex<()>,
    /// Optional closer for prompts abandoned by settled runs.
    prompts: parking_lot::Mutex<Option<Arc<dyn crate::CardPromptCloser>>>,
    /// What interrupts a live run. See [`crate::IssueRunStopper`].
    stopper: Arc<dyn crate::IssueRunStopper>,
    /// Runs already told to stop, so the driver's next pass does not tell
    /// them again: a cancel is asynchronous, and the row stays `Running`
    /// until its executor settles it. Pruned against what is still running,
    /// so it cannot outgrow the board.
    stopping: parking_lot::Mutex<std::collections::HashSet<IssueRunId>>,
}

/// The seam between recording a run and executing it. A channel rather
/// than a direct call: the executor lives in the agent runtime, which this
/// crate must not depend on.
pub type RunDispatch = Arc<dyn Fn(IssueRunRow) + Send + Sync>;

/// A dispatcher that records runs and starts nothing — the shape a
/// headless assembly and every store-level test wants.
pub fn no_dispatch() -> RunDispatch {
    Arc::new(|_run| {})
}

/// The [`no_dispatch`] of stoppers: nothing is executing in an assembly that
/// never dispatched anything, so there is nothing to interrupt.
pub fn no_stopper() -> Arc<dyn crate::IssueRunStopper> {
    struct StopNothing;

    #[async_trait::async_trait]
    impl crate::IssueRunStopper for StopNothing {
        async fn stop_run(
            &self,
            _session: &baybo_model::SessionId,
            _reason: crate::RunStopReason,
        ) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    Arc::new(StopNothing)
}

#[async_trait::async_trait]
impl crate::worktree::ProjectRepo for ProjectManager {
    async fn workdir(&self, project: &ProjectId) -> Option<PathBuf> {
        match self.store.get_project(project).await {
            Ok(Some(project)) => Some(PathBuf::from(project.workdir)),
            Ok(None) => None,
            Err(e) => {
                tracing::debug!(%project, error = %e, "could not resolve a project's repository");
                None
            }
        }
    }
}

impl ProjectManager {
    pub fn new(
        store: Arc<dyn ProjectStore>,
        agents: Arc<dyn AgentProfileStore>,
        blobs: Arc<dyn BlobStore>,
        paths: WorkspacePaths,
        events: Arc<dyn ProjectEvents>,
        dispatch: RunDispatch,
        stopper: Arc<dyn crate::IssueRunStopper>,
    ) -> Self {
        Self {
            store,
            agents,
            blobs,
            paths,
            events,
            dispatch,
            driving: tokio::sync::Mutex::new(()),
            leadless: parking_lot::Mutex::default(),
            merging: tokio::sync::Mutex::new(()),
            prompts: parking_lot::Mutex::default(),
            stopper,
            stopping: parking_lot::Mutex::default(),
        }
    }

    pub(crate) fn set_prompt_closer(&self, closer: Arc<dyn crate::CardPromptCloser>) {
        *self.prompts.lock() = Some(closer);
    }

    fn prompt_closer(&self) -> Option<Arc<dyn crate::CardPromptCloser>> {
        self.prompts.lock().clone()
    }

    /// Interrupt a live run, leaving its executor to settle the ledger.
    async fn stop_live_run(&self, run: &IssueRunRow, reason: crate::RunStopReason) {
        let Some(session) = run.session_id.as_ref() else {
            tracing::error!(
                run = %run.id,
                ?reason,
                "a run that never reached a session has nothing to interrupt"
            );
            return;
        };
        if let Err(e) = self.stopper.stop_run(session, reason).await {
            tracing::error!(run = %run.id, ?reason, error = %e, "could not stop a live run");
        }
    }

    /// The workspace this board's checkouts live under. Crate-internal:
    /// the rules that read it — the worktree layout, the build-artifact
    /// sweep — are this crate's, and nothing outside it gets a path.
    pub(crate) fn paths(&self) -> &WorkspacePaths {
        &self.paths
    }

    pub(crate) fn store(&self) -> &Arc<dyn ProjectStore> {
        &self.store
    }

    /// Return the dispatched run so unblock handling can avoid a duplicate.
    async fn dispatch_if_triggered(
        &self,
        transition: Transition,
        issue: &IssueRow,
        actor: &IssueActor,
    ) -> Option<IssueRunRow> {
        let trigger = triggers_run(transition)?;
        self.enqueue(issue, trigger, actor).await
    }

    async fn enqueue(
        &self,
        issue: &IssueRow,
        trigger: RunTrigger,
        actor: &IssueActor,
    ) -> Option<IssueRunRow> {
        self.enqueue_as(issue, trigger, issue.assignee.clone()?, actor)
            .await
    }

    /// [`enqueue`](Self::enqueue) for a run whose runner is not the card's
    /// assignee. The coordination triggers are that today
    /// ([`RunTrigger::is_coordination`]): the lead is woken *on* a card
    /// precisely because nobody is working it.
    async fn enqueue_as(
        &self,
        issue: &IssueRow,
        trigger: RunTrigger,
        agent: AgentProfileId,
        actor: &IssueActor,
    ) -> Option<IssueRunRow> {
        if let Some(why) = self.enqueue_refusal(issue, &agent).await {
            tracing::debug!(issue = issue.number, ?trigger, %agent, "{why}");
            return None;
        }
        let entry = ledger_entry(issue, trigger, agent);
        let headroom = self.headroom(&issue.project_id).await;
        if let Err(e) = self.release_holds(&issue.project_id, headroom).await {
            tracing::error!(project = %issue.project_id, error = %e, "could not release held runs");
        }
        let run = match self.store.enqueue_run(&entry).await {
            Ok(run) => run,
            Err(baybo_store::StorageError::Conflict(reason)) => {
                // The refusal is the guard working. What is not working is
                // the silence: the write that *implied* this run has
                // already committed — the card is in its new column, or
                // names its new agent, or says a stage opened — so the
                // card has to record that the run it promised was not
                // started. Every trigger, coordination included, with one
                // exception below: this entry is also the count of how
                // often one card's run slot was lost to *another* run, and
                // a count that saw half of those could not answer what it
                // exists to answer.
                //
                // Named for the row holding the slot, because "refused" is
                // not something an operator can act on and "run #4 has it"
                // is. A read failure still records the refusal — it
                // happened either way.
                let holder = match self.live_run(issue).await {
                    Ok(run) => run,
                    Err(e) => {
                        tracing::warn!(
                            issue = issue.number,
                            error = %e,
                            "could not read which run holds this card's slot"
                        );
                        None
                    }
                };
                if let Some(holder) = holder
                    .as_ref()
                    .filter(|holder| crate::runs::refused_itself(holder, actor))
                {
                    tracing::debug!(
                        issue = issue.number,
                        ?trigger,
                        run = %holder.id,
                        "an agent moved its own card from inside its own run; only its own run held the slot"
                    );
                    return None;
                }
                self.record(
                    issue,
                    IssueActor::System,
                    IssueEventBody::RunRefused {
                        trigger,
                        attempt: holder.map(|holder| holder.attempt),
                    },
                )
                .await;
                if trigger.is_coordination() {
                    tracing::debug!(
                        issue = issue.number,
                        %reason,
                        "issue already has a run in flight; not starting a second"
                    );
                } else {
                    // A refused human command is louder than a swept-up
                    // lead question: the drag or assignment itself
                    // succeeded, and what took the slot was most likely a
                    // live coordination run.
                    tracing::info!(
                        issue = issue.number,
                        ?trigger,
                        %reason,
                        "issue already has a run in flight; not starting a second"
                    );
                }
                return None;
            }
            Err(e) => {
                tracing::error!(issue = issue.number, error = %e, "could not record issue run");
                return None;
            }
        };
        self.events.run_changed(&issue.project_id, issue.number);

        // `Headroom` guarantees figures for an exhausted board.
        if let (true, Some(figures)) = (headroom.is_exhausted(), headroom.figures()) {
            // The **held** row, not the one `enqueue_run` answered with a
            // moment earlier: that one still says `Queued`, and it is what
            // the caller reports. Handed back unchanged it made the
            // follow-up log line call a stopped run "queued", and `retry`
            // answer 201 with a body saying the run was on its way.
            match self.store.hold_run(&run.id).await {
                Ok(Some(held)) => {
                    self.record(issue, IssueActor::System, figures.exhausted())
                        .await;
                    return Some(held);
                }
                // Either way the hold did not take, so the row is still
                // startable and is started: overspending by one run beats
                // stranding it. `None` is a compare-and-set that found the
                // row in another state, which nothing else should be able to
                // do to a row this call has only just written.
                Ok(None) => {
                    tracing::warn!(
                        issue = issue.number,
                        run = %run.id,
                        "a run was moved out of the queue before the budget could hold it"
                    );
                }
                Err(e) => {
                    tracing::error!(issue = issue.number, error = %e, "could not hold a run over budget");
                }
            }
        }
        (self.dispatch)(run.clone());
        Some(run)
    }

    /// Refusals checked before irreversible board changes; dedupe stays in storage.
    async fn enqueue_refusal(
        &self,
        issue: &IssueRow,
        agent: &AgentProfileId,
    ) -> Option<&'static str> {
        if !crate::runs::accepts_runs(issue) {
            return Some("the card is finished or cancelled; not starting a run on it");
        }
        // Framework eligibility can change after assignment.
        (self.can_run(agent).await != Some(true))
            .then_some("that agent cannot host an issue's session; not starting a run")
    }

    async fn headroom(&self, project: &ProjectId) -> Headroom {
        let (money, tokens) = match self.store.get_project(project).await {
            Ok(Some(row)) => (row.daily_budget, row.daily_budget_tokens),
            Ok(None) => return Headroom::Unlimited,
            Err(e) => {
                tracing::error!(%project, error = %e, "could not read the project's budget");
                return Headroom::Unlimited;
            }
        };
        // No ceiling means no query. The common case costs nothing.
        if money.is_none() && tokens.is_none() {
            return Headroom::Unlimited;
        }
        // Both meters must use the same spend snapshot.
        match self
            .store
            .spend_since(project, crate::budget::day_start(chrono::Utc::now()))
            .await
        {
            Ok(spent) => crate::budget::headroom(money, tokens, spent),
            Err(e) => {
                tracing::error!(%project, error = %e, "could not read the project's spend");
                Headroom::Unlimited
            }
        }
    }

    /// Start whatever this board is holding, if it has room again.
    pub(crate) async fn release_held_runs(&self, project: &ProjectId) -> Result<usize> {
        let headroom = self.headroom(project).await;
        self.release_holds(project, headroom).await
    }

    async fn release_holds(&self, project: &ProjectId, headroom: Headroom) -> Result<usize> {
        if headroom.is_exhausted() {
            return Ok(0);
        }
        // No ceiling at all: release everything, with nothing to report
        // against, rather than inventing figures for the timeline.
        let figures = headroom.figures();
        let held = self.store.held_runs(project).await?;
        let mut released = 0;
        for run in held {
            // Re-check the card because holds outlive their enqueue write.
            let Some(issue) = self.card_for(&run).await else {
                continue;
            };
            // What goes out is the **queued** row this just wrote. `held`
            // came off `held_runs`, so dispatching it would hand the
            // executor a row saying `Held` — and `IssueRunWaiter::enqueued`
            // documents its copy as reading `Queued` for the whole turn.
            let Some(queued) = self.store.release_run(&run.id).await? else {
                continue;
            };
            released += 1;
            self.events.run_changed(project, queued.number);
            if let Some(figures) = figures {
                self.record(&issue, IssueActor::System, figures.restored())
                    .await;
            }
            (self.dispatch)(queued);
        }
        Ok(released)
    }

    /// Drive every live board until `shutdown` resolves.
    ///
    /// The board's whole scheduler, and deliberately the *only* thing that
    /// calls [`drive`](Self::drive) in production. The alternative — a
    /// `drive` at the end of each of the seven writes that can change
    /// either side of the gap — makes every one of them load-bearing, so a
    /// new write path that forgets it leaves a board stuck until the
    /// process restarts. Here a forgotten anything costs at most one tick.
    ///
    /// The first tick is **not** consumed: a board whose queue moved while
    /// the process was down needs driving at boot, and that is the same
    /// pass rather than a special case.
    pub async fn run_driver(&self, shutdown: impl std::future::Future<Output = ()> + Send) {
        let mut ticker = tokio::time::interval(DRIVE_INTERVAL);
        // Skip rather than burst: a pass delayed by a slow tick is not a
        // reason to run several immediately, since each is level-triggered
        // and the last one would have done all the work anyway.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => {
                    tracing::debug!("board driver: shutting down");
                    return;
                }
                _ = ticker.tick() => self.drive_every_board().await,
            }
        }
    }

    async fn drive_every_board(&self) {
        let projects = match self.store.list_projects(false).await {
            Ok(projects) => projects,
            Err(e) => {
                tracing::error!(error = %e, "board driver could not list projects");
                return;
            }
        };
        for project in projects {
            self.drive(&project.id).await;
        }
    }

    /// Start whatever this board should be running and is not.
    ///
    /// Level-triggered and idempotent: it reads the board as it stands and
    /// fills the gap between what is executing and what the ceiling allows,
    /// so running it twice on an unchanged board does nothing the second
    /// time. That is what lets [`run_driver`](Self::run_driver) be the only
    /// caller — nothing has to *tell* it a card became ready.
    ///
    /// Returns how many cards it moved, for the tests and the log.
    pub async fn drive(&self, project: &ProjectId) -> usize {
        // One board at a time, process-wide. Reading the load and acting on
        // it is not atomic against the store, and three runs settling
        // together would otherwise each see the other two's slots as free
        // and promote into all of them. The critical section is a handful
        // of queries with no long await in it.
        let _driving = self.driving.lock().await;
        self.call_off_dead_holds(project).await;
        self.stop_runs_over_the_money_cap(project).await;
        match self.promotions(project).await {
            Ok(moved) => moved,
            Err(e) => {
                tracing::error!(%project, error = %e, "could not drive the board");
                0
            }
        }
    }

    /// Settle every hold whose card stopped accepting runs.
    ///
    /// Runs above every gate below it, and unconditionally.
    /// [`release_holds`](Self::release_holds) bails on an exhausted budget
    /// and [`promotions`](Self::promotions) bails on
    /// `max_parallel_issue_runs == 0`, so both of the ways an operator
    /// deliberately stops a board — spend the day's budget, set parallelism
    /// to zero — used to also stop the only sweep that settles a hold on a
    /// card they had already cancelled. A run nothing will ever start is
    /// not work waiting for a slot; left held it keeps the board's badge
    /// lit with something no operator action can discharge.
    async fn call_off_dead_holds(&self, project: &ProjectId) {
        let held = match self.store.held_runs(project).await {
            Ok(held) => held,
            Err(e) => {
                tracing::error!(%project, error = %e, "could not read the board's held runs");
                return;
            }
        };
        for run in &held {
            // The card, when there is one, is of no use here: what this
            // call is for is its other half — a run whose card stopped
            // accepting work is settled on the way past.
            let _ = self.card_for(run).await;
        }
    }

    /// Stop every run still working on a board that has spent its money.
    ///
    /// The enqueue gate decides whether the *next* run starts; this is the
    /// only thing that decides whether the current one continues, and
    /// without it a ceiling bounds nothing — one run's spend is unbounded,
    /// so a board overshoots by whatever its longest run costs. Money only:
    /// a token ceiling measures subscription plans, where the turn is paid
    /// for whether or not it is allowed to finish.
    ///
    /// Runs above [`promotions`](Self::promotions) for the same reason
    /// [`call_off_dead_holds`](Self::call_off_dead_holds) does — that
    /// function bails on an exhausted budget, which is exactly the state
    /// this one exists to act on.
    async fn stop_runs_over_the_money_cap(&self, project: &ProjectId) {
        let headroom = self.headroom(project).await;
        let working = match self.store.active_runs(project).await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::error!(%project, error = %e, "could not read the board's live runs");
                return;
            }
        };
        let running: Vec<IssueRunRow> = working
            .into_iter()
            .filter(|run| run.status == RunStatus::Running)
            .collect();
        // Pruned every pass, whether or not the board is over: a run that
        // settled is no longer something to skip, and the board is the only
        // bound on how big this can get.
        {
            let live: std::collections::HashSet<&IssueRunId> =
                running.iter().map(|run| &run.id).collect();
            self.stopping.lock().retain(|id| live.contains(id));
        }
        if !headroom.money_exhausted() {
            return;
        }
        for run in &running {
            if !self.stopping.lock().insert(run.id.clone()) {
                continue;
            }
            tracing::warn!(
                %project,
                issue = run.number,
                run = %run.id,
                "the board spent its money ceiling; stopping this run mid-flight"
            );
            // No `BudgetExhausted` entry here: that one says "held the run",
            // which is what the enqueue gate does and the opposite of this.
            // The card learns why from the settle its executor writes —
            // `RunSettled` carries the cancel's own note.
            self.stop_live_run(run, crate::RunStopReason::BudgetExhausted)
                .await;
        }
    }

    async fn promotions(&self, project: &ProjectId) -> Result<usize> {
        let row = self.get_project(project).await?;
        if row.archived_at.is_some() || row.max_parallel_issue_runs == 0 {
            return Ok(0);
        }
        // Owed work first, and *before* the load is read. A held run is a
        // card the board already promised to start, so it has the next slot
        // ahead of anything still sitting in Todo — and a held run is not in
        // `working`, so counting slots before releasing would hand those
        // slots out twice. The overshoot was real: the first promotion's
        // `enqueue` releases the holds itself, on its way past the budget
        // gate, so a board with three held runs and a rolled-over ceiling
        // ended up running six.
        self.release_held_runs(project).await?;

        let load = self.board_load(project).await?;
        // Whatever is still held after that release is held on budget, and
        // starting more work on top of it would mint cards that sit in In
        // Progress with nothing running under them — which is the one thing
        // that column is supposed to mean.
        if load.headroom.is_exhausted() {
            return Ok(0);
        }
        let issues = self.store.list_issues(project).await?;
        // Block-parked rows consume card slots but not execution capacity.
        let cards: HashMap<i64, &IssueRow> = issues.iter().map(|i| (i.number, i)).collect();
        let parked = |run: &IssueRunRow| {
            cards
                .get(&run.number)
                .is_some_and(|card| crate::runs::parked_by_a_block(run, card))
        };
        let recorded = || load.working.iter().chain(load.held.iter());
        let executing = load.working.iter().filter(|run| !parked(run)).count();
        let slots = row.max_parallel_issue_runs.saturating_sub(executing);
        // Held and block-parked rows still consume their card's run slot.
        let busy = crate::driver::busy(recorded().map(|run| run.number));
        // Lead questions care about execution, not merely a recorded row.
        let in_flight =
            crate::driver::busy(recorded().filter(|run| !parked(run)).map(|run| run.number));

        let slate = crate::driver::slate(&issues, &busy, slots);
        let mut moved = 0;
        for issue in &slate {
            if self.promote(issue).await {
                moved += 1;
            }
        }
        // After the promotions, and out of whatever room they left: work
        // somebody is already on outranks asking who should do work nobody
        // is on. A board with no capacity left says nothing rather than
        // queueing a question it cannot act on the answer to.
        if slots > moved {
            self.ask_the_lead(
                project,
                &issues,
                &BoardPass {
                    in_flight,
                    rules_changed_at: row.rules_changed_at,
                    promoted: moved,
                },
            )
            .await;
        }
        Ok(moved)
    }

    /// Move one ready card into In Progress and start it.
    ///
    /// Deliberately not [`move_issue`](Self::move_issue): that is the
    /// operator's door, and it would record the run as
    /// [`RunTrigger::Started`] — indistinguishable, in the execution log,
    /// from the drag this exists to save.
    ///
    /// The two writes are a move and an enqueue, in that order and not
    /// atomic, so **every reason the enqueue could refuse has to be asked
    /// before the move**. A card moved into In Progress whose run is then
    /// refused is stranded: it is no longer in Todo, so no later pass looks
    /// at it again.
    async fn promote(&self, issue: &IssueRow) -> bool {
        // Check every pre-write refusal before the irreversible move.
        let Some(assignee) = issue.assignee.clone() else {
            return false;
        };
        if let Some(why) = self.enqueue_refusal(issue, &assignee).await {
            tracing::debug!(
                issue = issue.number,
                %assignee,
                "{why}; leaving the card in Todo"
            );
            return false;
        }
        let column: Vec<i64> = match self.store.list_issues(&issue.project_id).await {
            Ok(issues) => {
                let mut in_progress: Vec<&IssueRow> = issues
                    .iter()
                    .filter(|other| other.status == IssueStatus::InProgress)
                    .collect();
                in_progress.sort_by_key(|other| (other.position, other.number));
                // Appended, not inserted: a card the board started is the
                // newest thing in flight, and the operator's own ordering
                // of what was already there is not the board's to rewrite.
                in_progress
                    .into_iter()
                    .map(|other| other.number)
                    .chain(std::iter::once(issue.number))
                    .collect()
            }
            Err(e) => {
                tracing::error!(issue = issue.number, error = %e, "could not read the In Progress column");
                return false;
            }
        };
        match self
            .store
            .move_issue(
                &issue.project_id,
                issue.number,
                IssueStatus::InProgress,
                &column,
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return false,
            Err(e) => {
                tracing::error!(issue = issue.number, error = %e, "could not promote a card");
                return false;
            }
        }
        let after = match self.get_issue(&issue.project_id, issue.number).await {
            Ok(after) => after,
            Err(e) => {
                tracing::error!(issue = issue.number, error = %e, "could not re-read a promoted card");
                return false;
            }
        };
        self.events
            .board_changed(&issue.project_id, Some(issue.number));
        self.record_diff(issue, &after, IssueActor::System).await;
        tracing::info!(
            project = %issue.project_id,
            issue = issue.number,
            "the board had room and started this card"
        );
        if self
            .enqueue(&after, RunTrigger::Promoted, &IssueActor::System)
            .await
            .is_some()
        {
            return true;
        }
        // The pre-checks passed and the enqueue still refused, so something
        // took the card's run slot between the load read and this write —
        // most likely a comment waking its assignee. Still loud, though the
        // stalled ask now catches the card once that interloping run
        // settles: an In Progress card with nothing under it should not
        // wait a whole run's length in silence.
        tracing::error!(
            project = %issue.project_id,
            issue = issue.number,
            "promoted a card and then could not start it; it is in In Progress with no run"
        );
        false
    }

    /// Wake the lead for the first card that needs its attention, if there
    /// is one it has not already been asked about as the card stands.
    ///
    async fn ask_the_lead(&self, project: &ProjectId, issues: &[IssueRow], pass: &BoardPass) {
        let in_flight = &pass.in_flight;
        let Some(lead) = self.coordinator(project).await else {
            return;
        };
        // Only the timeline records who opened a card, and every rule that
        // could move one out of Backlog asks. One board-level read, skipped
        // entirely when nothing is parked.
        let agent_opened = self.agent_opened_cards(project, issues).await;
        let questions = [
            (
                RunTrigger::Blocked,
                crate::driver::blocked(issues, in_flight, &lead),
                "asked the lead about a card a block has stopped",
            ),
            (
                RunTrigger::Review,
                crate::driver::awaiting_review(issues, in_flight, &lead),
                "asked the lead to arrange a review",
            ),
            (
                RunTrigger::Stalled,
                crate::driver::stalled(issues, in_flight, &lead),
                "asked the lead about a card whose work has stopped",
            ),
            (
                RunTrigger::Triage,
                crate::driver::awaiting_triage(issues, in_flight),
                "asked the lead to staff an unassigned card",
            ),
            // Last: parked work is the least urgent thing on a board, and
            // every question above it is about work already under way.
            (
                RunTrigger::Grooming,
                crate::driver::awaiting_grooming(issues, in_flight, &agent_opened, &lead),
                "asked the lead about a card an agent parked in Backlog",
            ),
        ];
        for (question, candidates, told) in questions {
            for issue in candidates {
                let runs = match self.store.list_runs(&issue.id).await {
                    Ok(runs) => runs,
                    Err(e) => {
                        tracing::error!(issue = issue.number, error = %e, "could not read a card's runs");
                        continue;
                    }
                };
                // A cancel is a stop that stands, and it stands against every
                // question — waking the lead about the card somebody just
                // stopped countermands them one tick later, whichever way the
                // wake is worded. `Blocked` is the one exception, and not a
                // loophole: the board settles the block-parked row
                // `Cancelled` itself in `stand_down_for_the_question` below,
                // so reading that as somebody's stop would make the question
                // refuse itself on the strength of its own preparation.
                if question != RunTrigger::Blocked && crate::driver::newest_run_was_cancelled(&runs)
                {
                    continue;
                }
                if crate::driver::already_asked(&issue, &runs, question) {
                    continue;
                }
                if question == RunTrigger::Blocked {
                    // Block authorship lives only on the timeline.
                    if !self.block_is_a_question(&issue).await {
                        continue;
                    }
                    // Preflight before settling the card's only row.
                    if let Some(why) = self.enqueue_refusal(&issue, &lead).await {
                        tracing::debug!(
                            issue = issue.number,
                            %lead,
                            "{why}; leaving the card's parked run where it is"
                        );
                        continue;
                    }
                    self.stand_down_for_the_question(&issue, &runs).await;
                }
                if self
                    .enqueue_as(&issue, question, lead.clone(), &IssueActor::System)
                    .await
                    .is_some()
                {
                    tracing::info!(%project, issue = issue.number, "{told}");
                    return;
                }
            }
        }
        self.tell_the_lead_the_board_ran_dry(project, issues, pass, &agent_opened, lead)
            .await;
    }

    /// Wake the lead about the **board** once it has run dry, having asked
    /// it about every card first.
    ///
    /// Reached only when every question above it declined, which is what
    /// makes it worth a run: each of those declines is the lead having
    /// already answered about that card, and a board where all of them have
    /// is a board that will now sit still for as long as it is left alone.
    /// The answers were not wrong when they were given — "wait for #8" is
    /// correct while #8 is running — they simply have no reader once the
    /// thing they were waiting for happens somewhere else.
    ///
    /// The store read comes **last**, behind both rules that can be answered
    /// from rows already in hand: it is the only board-wide query in the
    /// pass, and a board that is working — or one with nothing the question
    /// could be filed against — never pays for it.
    async fn tell_the_lead_the_board_ran_dry(
        &self,
        project: &ProjectId,
        issues: &[IssueRow],
        pass: &BoardPass,
        agent_opened: &std::collections::BTreeSet<i64>,
        lead: AgentProfileId,
    ) {
        if !crate::driver::ran_dry(issues, &pass.in_flight, agent_opened, pass.promoted) {
            return;
        }
        // No anchor is a board whose every live card is blocked, and each of
        // those blocks is a standing decision somebody made. There is
        // nothing here the board may act on, and `blocked` has already put
        // each of them to the lead once.
        let Some(anchor) = crate::driver::drain_anchor(issues, &pass.in_flight, agent_opened)
        else {
            return;
        };
        let marks = match self.store.drain_marks(project).await {
            Ok(marks) => marks,
            Err(e) => {
                tracing::error!(%project, error = %e, "could not read the board's drain marks");
                return;
            }
        };
        if crate::driver::nothing_has_happened_since_the_lead_looked(&marks, pass.rules_changed_at)
        {
            return;
        }
        if self
            .enqueue_as(&anchor, RunTrigger::BoardIdle, lead, &IssueActor::System)
            .await
            .is_some()
        {
            tracing::info!(
                %project,
                issue = anchor.number,
                "told the lead the board has run dry"
            );
        }
    }

    /// The cards on this board an agent opened.
    ///
    /// Empty when nothing sits in Backlog — the only column the answer is
    /// consulted for — and empty on a read failure, which leaves the
    /// operator's own parked cards alone rather than acting on them on a
    /// guess. On a board of nothing but parked cards that also means the
    /// drain question declines, which is the right way for this to fail:
    /// quiet, not busy.
    async fn agent_opened_cards(
        &self,
        project: &ProjectId,
        issues: &[IssueRow],
    ) -> std::collections::BTreeSet<i64> {
        if !issues.iter().any(|i| i.status == IssueStatus::Backlog) {
            return std::collections::BTreeSet::new();
        }
        match self.store.agent_opened_issues(project).await {
            Ok(numbers) => numbers.into_iter().collect(),
            Err(e) => {
                tracing::warn!(%project, error = %e, "could not read who opened this board's cards");
                std::collections::BTreeSet::new()
            }
        }
    }

    /// Settle a block-parked row after the caller preflights the lead run.
    async fn stand_down_for_the_question(&self, issue: &IssueRow, runs: &[IssueRunRow]) {
        let Some(parked) = crate::runs::unsettled(runs) else {
            return;
        };
        if !crate::runs::parked_by_a_block(parked, issue) {
            return;
        }
        tracing::info!(
            issue = issue.number,
            run = %parked.id,
            "the block parked this run; settling it so the lead can be asked to decide the block"
        );
        self.settle_off(parked, RunStatus::Cancelled, RUN_STOOD_DOWN_FOR_A_BLOCK)
            .await;
    }

    /// Whether the standing cancel is the operator's. Read failures answer
    /// yes, which leaves their stop where it is — the same direction
    /// [`block_is_a_question`] fails in, reached from its other side.
    async fn stopped_by_a_person(&self, issue: &IssueRow) -> bool {
        match self.store.list_events(&issue.id).await {
            Ok(events) => crate::driver::cancel_is_a_persons_stop(&events),
            Err(e) => {
                tracing::warn!(issue = issue.number, error = %e, "could not read who cancelled a card");
                true
            }
        }
    }

    /// Whether the block is an agent's question; read failures preserve operator stops.
    async fn block_is_a_question(&self, issue: &IssueRow) -> bool {
        match self.store.list_events(&issue.id).await {
            Ok(events) => crate::driver::block_is_an_agents_question(&events),
            Err(e) => {
                tracing::warn!(issue = issue.number, error = %e, "could not read who blocked a card");
                false
            }
        }
    }

    /// Return the coordinator, seeding a missing `@lead` when possible.
    async fn coordinator(&self, project: &ProjectId) -> Option<AgentProfileId> {
        if let Some(lead) = self.lead_of(project).await {
            return Some(lead);
        }
        if self.leadless.lock().contains(project) {
            return None;
        }
        match self.seed_lead(project).await {
            Ok(()) => {
                tracing::info!(%project, "this board had no @{LEAD_HANDLE}; seeded one");
                self.lead_of(project).await
            }
            Err(e) => {
                // Tombstones keep handles reserved.
                tracing::warn!(
                    %project,
                    error = %e,
                    "this board has no @{LEAD_HANDLE} and cannot be given one — hire an agent \
                     named \"{LEAD_HANDLE}\"; review, stalled, blocked and triage wakes are off \
                     until you do"
                );
                self.leadless.lock().insert(project.clone());
                None
            }
        }
    }

    /// The board's coordinator, by the handle every board issues it.
    async fn lead_of(&self, project: &ProjectId) -> Option<AgentProfileId> {
        match self.team(project).await {
            Ok(rows) => rows
                .into_iter()
                .find(|row| {
                    row.team
                        .as_ref()
                        .is_some_and(|team| team.handle.as_str() == LEAD_HANDLE)
                })
                .map(|row| row.id),
            Err(e) => {
                tracing::error!(%project, error = %e, "could not find the board's lead");
                None
            }
        }
    }

    /// Apply the recovery verdict and return only a runnable card.
    async fn card_for(&self, run: &IssueRunRow) -> Option<IssueRow> {
        let card = match self.store.get_issue(&run.project_id, run.number).await {
            Ok(card) => card,
            Err(e) => {
                tracing::error!(issue = run.number, error = %e, "could not read the card a recorded run belongs to; leaving the run where it is");
                return None;
            }
        };
        match crate::runs::verdict(run, card.as_ref(), chrono::Utc::now()) {
            crate::runs::Verdict::Stands => card,
            crate::runs::Verdict::Park(why) => {
                tracing::debug!(issue = run.number, run = %run.id, "{why}");
                None
            }
            crate::runs::Verdict::CallOff => {
                self.settle_off(run, RunStatus::Cancelled, call_off_reason(run))
                    .await;
                None
            }
            crate::runs::Verdict::GiveUp { bound, reason } => {
                tracing::warn!(
                    issue = run.number,
                    run = %run.id,
                    ?bound,
                    resumes = run.resumes,
                    "the board gave up on a recorded run"
                );
                self.settle_off(run, RunStatus::Failed, reason).await;
                None
            }
        }
    }

    async fn settle_off(&self, run: &IssueRunRow, status: RunStatus, reason: &str) {
        let prompts = self.prompt_closer();
        if let Err(e) = crate::settle::settle_run(
            &self.store,
            &self.events,
            prompts.as_deref(),
            run,
            IssueActor::System,
            status,
            Some(reason),
        )
        .await
        {
            tracing::error!(issue = run.number, error = %e, "could not settle a run the board is done with");
        }
    }

    /// Take a recorded run for execution and say so on the card.
    ///
    /// `Ok(false)` means another executor claimed it first and this one
    /// must not run it — the claim is the only thing standing between a
    /// re-dispatched row and two agents on one card.
    pub async fn start_run(&self, run: &IssueRunRow, session: &SessionId) -> Result<bool> {
        if !self.store.claim_run(&run.id, session).await? {
            return Ok(false);
        }
        // Resumes emit a run change but reuse their original `RunStarted` event.
        self.events.run_changed(&run.project_id, run.number);
        if crate::runs::ever_ran(run) {
            return Ok(true);
        }
        crate::settle::record(
            &self.store,
            &self.events,
            run,
            IssueActor::Agent(run.agent_id.clone()),
            IssueEventBody::RunStarted {
                run_id: run.id.clone(),
                attempt: run.attempt,
                trigger: run.trigger,
            },
        )
        .await;
        Ok(true)
    }

    /// Close out a run its executor has finished with: settle the ledger,
    /// surface the branch its work is on, and follow up if somebody spoke
    /// while it worked.
    ///
    /// The three are in this order and not the caller's business. The
    /// branch is read before the follow-up because a follow-up enqueues
    /// another run against the same checkout, and the settle comes first so
    /// that a card whose branch cannot be read still stops shimmering.
    ///
    /// `briefed_at` is [`IssueRunEvent::briefed_at`](crate::IssueRunEvent) —
    /// what this run was told, as an instant.
    pub async fn finish_run(
        &self,
        run: &IssueRunRow,
        checkout: &Path,
        briefed_at: DateTime<Utc>,
        outcome: RunOutcome,
    ) {
        // Under the driver's lock, because the gap between the settle and
        // the follow-up enqueue below is otherwise a window a drive tick
        // can win: the just-settled card reads as stalled (or as awaiting
        // review), the lead's coordination run takes the card's one run
        // slot, and the comment follow-up this run owes is refused —
        // swallowing the nudge and billing a lead run in its place.
        let _driving = self.driving.lock().await;
        let prompts = self.prompt_closer();
        if let Err(e) = crate::settle::settle_run(
            &self.store,
            &self.events,
            prompts.as_deref(),
            run,
            IssueActor::Agent(run.agent_id.clone()),
            outcome.status,
            outcome.error.as_deref(),
        )
        .await
        {
            tracing::error!(run = %run.id, error = %e, "could not settle a finished run; the boot sweep will retry it");
        }
        self.record_branch(run, checkout).await;
        self.wake_after_run(run, briefed_at, outcome.stopped_by_a_human)
            .await;
    }

    /// Record the branch a run's work is on, once there is work on it.
    ///
    /// On a board that does not merge its own work, this ref is the artefact
    /// it hands the operator; on one that does,
    /// [`merge_issue_branch`](Self::merge_issue_branch) records it *before*
    /// merging, because a merged branch is zero commits ahead and the guard
    /// below would then never record it at all. It is read from the checkout
    /// rather than derived from the
    /// title so that a retitle mid-run cannot rename it, and it falls back
    /// to the name the tree was cut with so that a card finished *before*
    /// its run settled — which reclaims the tree — still surfaces one.
    async fn record_branch(&self, run: &IssueRunRow, checkout: &Path) {
        let (Ok(Some(issue)), Ok(Some(project))) = (
            self.store.get_issue(&run.project_id, run.number).await,
            self.store.get_project(&run.project_id).await,
        ) else {
            return;
        };
        if issue.branch.is_some() {
            return;
        }
        let branch = self.branch_worked_on(checkout, &issue).await;
        // Asked of the repository, not the checkout: the tree may already
        // have been reclaimed, while the ref it left behind has not.
        if crate::worktree::commits_ahead(Path::new(&project.workdir), &branch)
            .await
            .is_none_or(|ahead| ahead == 0)
        {
            return;
        }
        match self.store.set_issue_branch(&issue.id, &branch).await {
            Ok(true) => self
                .events
                .board_changed(&issue.project_id, Some(issue.number)),
            Ok(false) => {}
            Err(e) => {
                tracing::error!(issue = issue.number, error = %e, "could not record the issue's branch")
            }
        }
    }

    /// The branch an issue's work is on: the checkout's own, or — once the
    /// checkout is gone — the name it was cut with. Never re-derived from
    /// the *current* title while the tree can answer, because a retitle
    /// would otherwise name a ref git still knows by the old one.
    async fn branch_worked_on(&self, root: &Path, issue: &IssueRow) -> String {
        match crate::worktree::branch_of(root).await {
            Some(branch) => branch,
            None => crate::worktree::branch_name(issue.number, &issue.title),
        }
    }

    /// Start another run if somebody asked for one while this one worked.
    ///
    /// The window is the instant the run's brief was **read**, which is
    /// neither of the two instants the ledger row carries. `created_at` is
    /// the enqueue, and a run held over budget can sit there a day before
    /// its brief is cut — waking on all of it re-instructs the agent to
    /// redo what it just did. `started_at` is the claim, which happens
    /// *after* the brief: a `git worktree add` and a trip through the run
    /// channel sit in between, and anything said in that gap is in neither
    /// the brief nor a window bounded by the claim, so nothing would ever
    /// pick it up.
    ///
    /// Comments by whoever would run next — the card's current assignee —
    /// are skipped as well: an agent reporting progress is not somebody
    /// asking it for more. A lead's coordination-run comment is, which is
    /// how a lead's settle becomes the assignee's wake.
    async fn wake_after_run(
        &self,
        run: &IssueRunRow,
        briefed_at: DateTime<Utc>,
        stopped_by_a_human: bool,
    ) -> Option<IssueRunRow> {
        if stopped_by_a_human {
            tracing::debug!(run = %run.id, "somebody stopped this run; not starting a follow-up on it");
            return None;
        }
        if !self.was_told_something_during(run, briefed_at).await {
            return None;
        }
        let next = self.wake_on_comment(&run.project_id, run.number).await;
        match &next {
            Some(next) if next.status == RunStatus::Held => {
                tracing::info!(run = %run.id, next = %next.id, "a comment arrived mid-run; the follow-up is held until the board has budget")
            }
            Some(next) => {
                tracing::info!(run = %run.id, next = %next.id, "a comment arrived mid-run; queued a follow-up")
            }
            None => {
                tracing::debug!(run = %run.id, "a comment arrived mid-run, but the issue is no longer listening")
            }
        }
        next
    }

    /// Whether the run recorded qualifying card activity after its brief.
    pub async fn run_left_a_mark(&self, run: &IssueRunRow, briefed_at: DateTime<Utc>) -> bool {
        let own = IssueActor::Agent(run.agent_id.clone());
        match self.store.events_since(&run.issue_id, briefed_at).await {
            Ok(events) => events
                .iter()
                .any(|e| e.actor == own && crate::timeline::left_a_mark(&e.body)),
            Err(e) => {
                // A failed read cannot support a negative claim.
                tracing::warn!(run = %run.id, error = %e, "could not read what a run left on its card");
                true
            }
        }
    }

    async fn was_told_something_during(
        &self,
        run: &IssueRunRow,
        briefed_at: DateTime<Utc>,
    ) -> bool {
        // The follow-up this decides would run as the card's CURRENT
        // assignee, so that is who the filter protects from waking itself:
        // for an ordinary run the two are the same profile, and for a
        // lead's coordination run they differ — which is exactly right,
        // because the lead commenting "please continue" on a stalled card
        // *is* somebody asking the assignee for more.
        let next_runner = match self.store.get_issue(&run.project_id, run.number).await {
            Ok(Some(issue)) => issue.assignee,
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "could not read the card a run finished on");
                None
            }
        };
        let own = IssueActor::Agent(next_runner.unwrap_or_else(|| run.agent_id.clone()));
        let since_the_brief = match self.store.events_since(&run.issue_id, briefed_at).await {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "could not check for comments left during the run");
                return false;
            }
        };
        if crate::comments::somebody_asked_for_more(&since_the_brief, &own) {
            return true;
        }
        // Read the wider block window only when this run lifted one.
        crate::driver::a_block_was_lifted(&since_the_brief)
            && self
                .was_told_something_while_parked(run, briefed_at, &own)
                .await
    }

    /// Check the parked window that the ordinary brief window cannot see.
    async fn was_told_something_while_parked(
        &self,
        run: &IssueRunRow,
        briefed_at: DateTime<Utc>,
        own: &IssueActor,
    ) -> bool {
        let events = match self.store.list_events(&run.issue_id).await {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "could not read what was said while a card was blocked");
                return false;
            }
        };
        let Some(blocked_at) = crate::driver::block_standing_at(&events, briefed_at) else {
            return false;
        };
        crate::comments::somebody_asked_for_more(
            events.iter().filter(|e| e.created_at >= blocked_at),
            own,
        )
    }

    /// Redrive a parked row or deliver its comments; held rows pass through the budget gate.
    async fn redrive_after_unblock(&self, issue: &IssueRow, started: Option<&IssueRunRow>) {
        let runs = match self.store.list_runs(&issue.id).await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::error!(issue = issue.number, error = %e, "could not read the unblocked card's runs");
                return;
            }
        };
        let Some(parked) = crate::runs::unsettled(&runs).cloned() else {
            self.deliver_what_was_said_while_parked(issue).await;
            return;
        };
        if started.is_some_and(|started| started.id == parked.id) {
            return;
        }
        // Re-check current gates; the run claim resolves concurrent redispatches.
        if self.card_for(&parked).await.is_none() {
            return;
        }
        match parked.status {
            RunStatus::Queued => {
                tracing::info!(
                    issue = issue.number,
                    run = %parked.id,
                    "unblocked; handing its parked run back out"
                );
                (self.dispatch)(parked);
            }
            RunStatus::Held => {
                if let Err(e) = self.release_held_runs(&issue.project_id).await {
                    tracing::error!(issue = issue.number, error = %e, "could not release held runs after an unblock");
                }
            }
            // A running row remains executor-owned; settlement delivers parked comments.
            _ => {}
        }
    }

    /// Deliver block-window comments when no parked row remains to read them.
    async fn deliver_what_was_said_while_parked(&self, issue: &IssueRow) {
        let Some(assignee) = issue.assignee.clone() else {
            return;
        };
        let events = match self.store.list_events(&issue.id).await {
            Ok(events) => events,
            Err(e) => {
                tracing::warn!(issue = issue.number, error = %e, "could not read what was said while a card was blocked");
                return;
            }
        };
        let Some(blocked_at) = crate::driver::blocked_at(&events) else {
            return;
        };
        let while_parked = events.iter().filter(|e| e.created_at >= blocked_at);
        if !crate::comments::somebody_asked_for_more(while_parked, &IssueActor::Agent(assignee)) {
            return;
        }
        tracing::info!(
            issue = issue.number,
            "unblocked; delivering what was said while the card was parked"
        );
        self.wake_if_listening(issue).await;
    }

    async fn record(&self, issue: &IssueRow, actor: IssueActor, body: IssueEventBody) {
        let entry = NewIssueEvent {
            issue_id: issue.id.clone(),
            project_id: issue.project_id.clone(),
            number: issue.number,
            actor,
            body,
        };
        match self.store.append_event(&entry).await {
            Ok(_) => self
                .events
                .timeline_changed(&issue.project_id, issue.number),
            Err(e) => {
                tracing::error!(
                    issue = issue.number,
                    error = %e,
                    "could not record a timeline entry; the change itself stands"
                );
            }
        }
    }

    /// Append one entry to an issue's timeline, addressed the way the
    /// board addresses everything: by number.
    pub async fn record_event(
        &self,
        project: &ProjectId,
        number: i64,
        actor: IssueActor,
        body: IssueEventBody,
    ) {
        match self.get_issue(project, number).await {
            Ok(issue) => self.record(&issue, actor, body).await,
            Err(e) => {
                tracing::warn!(issue = number, error = %e, "no issue to record a timeline entry on")
            }
        }
    }

    async fn record_diff(&self, before: &IssueRow, after: &IssueRow, actor: IssueActor) {
        for body in crate::timeline::diff_events(before, after) {
            self.record(after, actor.clone(), body).await;
        }
    }

    async fn reclaim_if_finished(&self, before: &IssueRow, after: &IssueRow, actor: IssueActor) {
        if crate::stages::is_finished(before) || !crate::stages::is_finished(after) {
            return;
        }
        let Ok(Some(project)) = self.store.get_project(&after.project_id).await else {
            return;
        };
        let root = crate::worktree::worktree_root(&self.paths, &after.project_id, after.number);
        let branch = self.branch_worked_on(&root, after).await;
        match crate::worktree::reclaim(Path::new(&project.workdir), &root, &branch).await {
            Ok(crate::worktree::Reclaimed::Removed { branch_deleted }) => {
                self.record(
                    after,
                    actor,
                    IssueEventBody::WorktreeReclaimed { branch_deleted },
                )
                .await;
            }
            Ok(crate::worktree::Reclaimed::Kept { reason }) => {
                self.record(after, actor, IssueEventBody::WorktreeKept { reason })
                    .await;
            }
            // Nothing was there. Silent on purpose: an issue that never ran
            // has no worktree, and saying so on every card that reaches
            // Done would be noise on the entries that matter.
            Ok(crate::worktree::Reclaimed::Absent) => {}
            Err(e) => {
                tracing::error!(issue = after.number, error = %e, "could not reclaim the worktree");
            }
        }
    }

    /// Say something on an issue, and reach whoever should hear it.
    ///
    /// A comment may be nothing but files. "Here" with a screenshot under it
    /// is a real thing to say on a card, and refusing it would push the
    /// operator into typing a word they do not mean just to get the picture
    /// delivered.
    pub async fn comment(
        &self,
        project: &ProjectId,
        number: i64,
        actor: IssueActor,
        text: &str,
        attachments: &[AttachmentRequest],
    ) -> Result<IssueEventRow> {
        self.comment_inner(project, number, actor, text, attachments, None)
            .await
    }

    /// Record a client comment exactly once. A retry returns the original
    /// timeline row and repeats none of the wake, mention, or uncancel side
    /// effects that followed its first insertion.
    pub async fn comment_idempotent(
        &self,
        project: &ProjectId,
        number: i64,
        actor: IssueActor,
        text: &str,
        attachments: &[AttachmentRequest],
        client_msg_id: IssueEventClientMsgId,
    ) -> Result<IssueEventRow> {
        self.comment_inner(
            project,
            number,
            actor,
            text,
            attachments,
            Some(client_msg_id),
        )
        .await
    }

    async fn comment_inner(
        &self,
        project: &ProjectId,
        number: i64,
        actor: IssueActor,
        text: &str,
        attachments: &[AttachmentRequest],
        client_msg_id: Option<IssueEventClientMsgId>,
    ) -> Result<IssueEventRow> {
        let issue = self.get_issue(project, number).await?;
        // Idempotency replays the original response even if the board became
        // archived after the first request landed. Re-running current write
        // validation would turn an already-durable comment into a failure.
        if let Some(client_msg_id) = &client_msg_id
            && let Some(entry) = self
                .store
                .event_by_client_msg_id(&issue.id, client_msg_id)
                .await?
        {
            return Ok(entry);
        }
        self.writable_project(project).await?;
        let text = text.trim();
        let attachments = attachments::resolve(&self.blobs, attachments).await?;
        if text.is_empty() && attachments.is_empty() {
            return Err(ProjectError::invalid("text", "a comment cannot be empty"));
        }
        let event = NewIssueEvent {
            issue_id: issue.id.clone(),
            project_id: project.clone(),
            number,
            actor: actor.clone(),
            body: IssueEventBody::Comment {
                text: text.to_owned(),
                attachments,
            },
        };
        let entry = match client_msg_id {
            Some(client_msg_id) => match self
                .store
                .append_event_idempotent(&event, &client_msg_id)
                .await?
            {
                IssueEventAppendOutcome::Inserted(entry) => entry,
                IssueEventAppendOutcome::Existing(entry) => return Ok(entry),
            },
            None => self.store.append_event(&event).await?,
        };
        self.events.timeline_changed(project, number);
        // After the entry, not before: the comment is the reason the card is
        // coming back, and a timeline that reopened it first would read as
        // the operator explaining a decision they had already taken.
        let issue = self
            .take_the_cancel_back(&issue, &actor)
            .await
            .unwrap_or(issue);

        let issue = match self.mention_assignment(project, &issue, text).await {
            Some(assignee) => {
                match self
                    .update_issue(
                        project,
                        number,
                        actor,
                        IssueUpdate {
                            assignee: Some(Some(assignee)),
                            ..IssueUpdate::default()
                        },
                        None,
                    )
                    .await
                {
                    // The assign may itself have started a run; re-read so
                    // the delivery decision below sees that.
                    Ok(after) => after,
                    Err(e) => {
                        tracing::debug!(issue = number, error = %e, "a mention named somebody who cannot take this card");
                        issue
                    }
                }
            }
            None => issue,
        };

        self.wake_if_listening(&issue).await;
        Ok(entry)
    }

    /// A person commenting on a cancelled card is asking for it back.
    ///
    /// Without this the comment is a dead end:
    /// [`comments::comment_delivery`](crate::comments) answers `RecordOnly`
    /// for a cancelled card, so the operator types into a card nothing will
    /// ever read, and the only way back is a second, different gesture.
    ///
    /// **A person's comment only.** An agent's would reach around
    /// [`driver::cancel_is_a_persons_stop`](crate::driver) rather than
    /// through it — saying a word on the card would take back a stop the
    /// same agent is refused when it asks directly — and the board reviving
    /// its own cancel is the board acting on nobody's authority.
    ///
    /// Written through `update_issue` like every other reversal, so the
    /// `Uncancelled` entry, the board event and the enqueue gate all happen
    /// once and in one place. A failure leaves the comment standing: the
    /// operator said something, and losing that to a write they did not ask
    /// for would be the worse half.
    async fn take_the_cancel_back(&self, issue: &IssueRow, actor: &IssueActor) -> Option<IssueRow> {
        if issue.cancelled_at.is_none() || !matches!(actor, IssueActor::User) {
            return None;
        }
        match self
            .update_issue(
                &issue.project_id,
                issue.number,
                actor.clone(),
                IssueUpdate {
                    cancelled: Some(false),
                    ..IssueUpdate::default()
                },
                None,
            )
            .await
        {
            Ok(after) => Some(after),
            Err(e) => {
                tracing::error!(issue = issue.number, error = %e, "could not take back the cancel a comment asked for");
                None
            }
        }
    }

    /// Start the run a comment asked for, on an issue whose answer had to
    /// wait.
    pub async fn wake_on_comment(&self, project: &ProjectId, number: i64) -> Option<IssueRunRow> {
        self.writable_project(project).await.ok()?;
        let issue = self.get_issue(project, number).await.ok()?;
        self.wake_if_listening(&issue).await
    }

    async fn wake_if_listening(&self, issue: &IssueRow) -> Option<IssueRunRow> {
        if self.delivery_for(issue).await != CommentDelivery::Wake {
            return None;
        }
        self.enqueue(issue, RunTrigger::Comment, &IssueActor::System)
            .await
    }

    /// Resolve comment mentions without overriding a block.
    async fn mention_assignment(
        &self,
        project: &ProjectId,
        issue: &IssueRow,
        text: &str,
    ) -> Option<AgentProfileId> {
        let handle = crate::mentions::assigns_to(issue.assignee.is_some(), text)?;
        if !crate::driver::board_may_start(issue) {
            tracing::debug!(
                issue = issue.number,
                %handle,
                "a mention named somebody on a card a block has stopped; recording it and \
                 putting nobody on it"
            );
            return None;
        }
        let team = self.agents.list_team(project).await.ok()?;
        team.into_iter()
            .find(|row| {
                row.team
                    .as_ref()
                    .is_some_and(|t| t.handle.as_str() == handle.as_str())
            })
            .map(|row| row.id)
    }

    /// What this comment will do besides being recorded.
    pub async fn comment_delivery(
        &self,
        project: &ProjectId,
        number: i64,
    ) -> Result<CommentDelivery> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.delivery_for(&issue).await)
    }

    async fn live_run(&self, issue: &IssueRow) -> Result<Option<IssueRunRow>> {
        Ok(self
            .store
            .list_runs(&issue.id)
            .await?
            .into_iter()
            .find(crate::runs::is_unsettled))
    }

    async fn delivery_for(&self, issue: &IssueRow) -> CommentDelivery {
        let live = match self.live_run(issue).await {
            Ok(run) => run.map(|run| run.status),
            Err(e) => {
                // Fail towards doing nothing: a spurious wake starts an
                // agent on work nobody asked it to redo.
                tracing::error!(issue = issue.number, error = %e, "could not read runs for comment delivery");
                return CommentDelivery::RecordOnly;
            }
        };
        crate::comments::comment_delivery(issue, live)
    }

    /// One issue's timeline, oldest first.
    pub async fn timeline(&self, project: &ProjectId, number: i64) -> Result<Vec<IssueEventRow>> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.store.list_events(&issue.id).await?)
    }

    /// Where a reader opening this card should land: its oldest entry the
    /// operator has not seen, or `None` on a card with nothing new.
    ///
    /// The resolved id, never the read cursor — see
    /// [`ProjectStore::first_unread_event`](baybo_store::project::ProjectStore::first_unread_event).
    /// A client handed the cursor would decide for itself which rows are
    /// new, and the divider it drew would be free to disagree with the
    /// unread badge that sent the operator here.
    pub async fn first_unread_event(
        &self,
        project: &ProjectId,
        number: i64,
    ) -> Result<Option<IssueEventId>> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.store.first_unread_event(&issue.id).await?)
    }

    /// Every run of one issue, newest first. The unpriced read, for callers
    /// choosing which run to continue rather than showing what one cost.
    pub async fn list_runs(&self, project: &ProjectId, number: i64) -> Result<Vec<IssueRunRow>> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.store.list_runs(&issue.id).await?)
    }

    /// Every run of one issue, newest first, each already priced, plus what
    /// the card has cost in total. One call rather than a run list and a
    /// pricing recipe: the execution log and the rail's total are the same
    /// fact at two altitudes, and a caller that summed the rows itself
    /// would be the second place that decides what a card cost.
    pub async fn run_log(&self, project: &ProjectId, number: i64) -> Result<IssueRunLog> {
        let issue = self.get_issue(project, number).await?;
        let runs = self.store.list_runs(&issue.id).await?;
        let spend: std::collections::HashMap<_, _> = self
            .store
            .run_spend(&issue.id)
            .await?
            .into_iter()
            .map(|row| (row.run_id, row.spend))
            .collect();
        let runs: Vec<RunWithSpend> = runs
            .into_iter()
            .map(|run| {
                let spend = spend.get(&run.id).copied().unwrap_or_default();
                RunWithSpend { run, spend }
            })
            .collect();
        let total = runs.iter().map(|row| row.spend).sum();
        Ok(IssueRunLog { runs, total })
    }

    /// Stop an issue's run.
    ///
    /// The whole job, not half of it: a run that reached a session is
    /// interrupted through [`IssueRunStopper`](crate::IssueRunStopper) and
    /// settled by the executor watching its turn, and one that never
    /// reached one is settled here. Callers used to be handed the
    /// `SessionId` and left to do the interrupting, which put "how a run is
    /// stopped" in two places — and only one of them learned about the
    /// money ceiling.
    pub async fn cancel_run(&self, project: &ProjectId, number: i64) -> Result<()> {
        let issue = self.get_issue(project, number).await?;
        let Some(run) = self.live_run(&issue).await? else {
            return Err(ProjectError::invalid(
                "run",
                "nothing is running on this issue",
            ));
        };
        // A run with a session is stopped by cancelling its turn, and the
        // executor settles it on the way out. Only a run that never reached
        // one — `Queued` or `Held` — is settled here.
        if run.status == RunStatus::Running && run.session_id.is_some() {
            self.stop_live_run(&run, crate::RunStopReason::Operator)
                .await;
            return Ok(());
        }
        let prompts = self.prompt_closer();
        crate::settle::settle_run(
            &self.store,
            &self.events,
            prompts.as_deref(),
            &run,
            IssueActor::User,
            RunStatus::Cancelled,
            Some(RUN_CANCELLED_BEFORE_STARTING),
        )
        .await?;
        Ok(())
    }

    /// Run an issue again, unless another run or block already holds the card.
    pub async fn retry_run(&self, project: &ProjectId, number: i64) -> Result<IssueRunRow> {
        self.writable_project(project).await?;
        let issue = self.get_issue(project, number).await?;
        let Some(assignee) = issue.assignee.clone() else {
            return Err(ProjectError::invalid(
                "assignee",
                "an issue with nobody on it cannot be run",
            ));
        };
        self.validate_assignee(project, &assignee).await?;
        if !crate::runs::accepts_runs(&issue) {
            return Err(ProjectError::invalid(
                "issue",
                if issue.cancelled_at.is_some() {
                    "this issue was cancelled — reopen it before running it again"
                } else {
                    "this issue is done — move it back into the board before running it again"
                },
            ));
        }
        // Check liveness first because finished cards may retain a block reason.
        if !crate::driver::board_may_start(&issue) {
            return Err(ProjectError::invalid("issue", RETRY_ON_A_BLOCKED_CARD));
        }
        let held = self
            .live_run(&issue)
            .await?
            .filter(|run| run.status == RunStatus::Held);

        if let Some(run) = self
            .enqueue(&issue, RunTrigger::Retry, &IssueActor::User)
            .await
        {
            return Ok(run);
        }
        match held {
            Some(held) => match self.live_run(&issue).await? {
                Some(run) if run.id == held.id && run.status != RunStatus::Held => Ok(run),
                _ => Err(ProjectError::Conflict(held_run_refusal(
                    self.headroom(&issue.project_id).await.figures(),
                ))),
            },
            None => Err(ProjectError::Conflict(
                "this issue already has a run".to_owned(),
            )),
        }
    }

    /// The unfinished runs of one board — which cards are working.
    pub async fn active_runs(&self, project: &ProjectId) -> Result<Vec<IssueRunRow>> {
        self.get_project(project).await?;
        Ok(self.store.active_runs(project).await?)
    }

    /// Return orphaned runs to the queue and hand each back for dispatch.
    /// Called once per process start, from a task that races the server
    /// coming up rather than one that finishes before it: a `running` row
    /// whose actor died with the process is work that never finished.
    pub async fn resume_unsettled_runs(&self) -> Result<usize> {
        // Record interruptions even on archived boards; dispatch waits for restore.
        for run in self.store.requeue_unsettled().await? {
            crate::settle::record(
                &self.store,
                &self.events,
                &run,
                IssueActor::System,
                IssueEventBody::RunInterrupted {
                    run_id: run.id.clone(),
                    attempt: run.attempt,
                    resumes: run.resumes,
                },
            )
            .await;
            self.events.run_changed(&run.project_id, run.number);
        }
        let mut count = 0;
        for project in self.store.list_projects(false).await? {
            match self.resume_project_runs(&project.id).await {
                Ok(resumed) => count += resumed,
                Err(e) => {
                    tracing::error!(project = %project.id, error = %e, "could not resume a board's runs")
                }
            }
        }
        Ok(count)
    }

    async fn resume_project_runs(&self, project: &ProjectId) -> Result<usize> {
        self.writable_project(project).await?;
        let mut count = 0;
        for run in self.store.active_runs(project).await? {
            if run.status != RunStatus::Queued {
                continue;
            }
            // The shared verdict parks blocked rows and settles exhausted recovery.
            if self.card_for(&run).await.is_none() {
                continue;
            }
            count += 1;
            (self.dispatch)(run);
        }
        match self.release_held_runs(project).await {
            Ok(released) => count += released,
            Err(e) => {
                tracing::error!(project = %project, error = %e, "could not release held runs")
            }
        }
        Ok(count)
    }

    pub async fn create_project(&self, new: NewProject) -> Result<ProjectRow> {
        let name = validate_name(&new.name)?;
        validate_ceilings(new.daily_budget, new.daily_budget_tokens)?;
        let id = ProjectId::generate();

        let workdir = match new.workdir.as_deref().map(str::trim) {
            Some(given) if !given.is_empty() => {
                validate_workdir(given, &self.paths)?;
                let path = Path::new(given);
                if !path.join(".git").exists() {
                    return Err(ProjectError::invalid(
                        "workdir",
                        format!(
                            "{given} is not a git repository. Leave the field empty to have \
                             one created under the workspace instead."
                        ),
                    ));
                }
                given.to_owned()
            }
            _ => self.materialise_workdir(&name).await?,
        };

        self.seed_lead(&id).await?;

        let now = chrono::Utc::now();
        let row = ProjectRow {
            id,
            name,
            description: new.description.trim().to_owned(),
            workdir,
            daily_budget: new.daily_budget,
            daily_budget_tokens: new.daily_budget_tokens,
            max_parallel_issue_runs: new
                .max_parallel_issue_runs
                .unwrap_or(DEFAULT_MAX_PARALLEL_ISSUE_RUNS),
            agents_may_merge: new.agents_may_merge,
            // A board opens with the rules it was created under, so nothing
            // on it has been answered under any others yet.
            rules_changed_at: now,
            // Never read, so a board's first agent comment is unread even
            // if it lands before anybody opens it.
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        self.store.create_project(&row).await?;
        self.events.project_changed(&row.id);
        Ok(row)
    }

    async fn seed_lead(&self, project: &ProjectId) -> Result<()> {
        // A compile-time literal, so this can only fail if somebody edits
        // `LEAD_HANDLE` into something the grammar refuses — which the test
        // below catches long before a project is ever opened.
        let handle = AgentHandle::parse(LEAD_HANDLE)
            .map_err(|e| anyhow::anyhow!("LEAD_HANDLE is not a valid handle: {e}"))?;
        let id = AgentProfileId::generate_project();
        baybo_workspace::ensure_named_persona_layout(
            &self.paths,
            id.as_str(),
            baybo_workspace::prompt::PROJECT_LEAD_SOUL_TEMPLATE,
            LEAD_HANDLE,
        )
        .await
        .map_err(|e| anyhow::anyhow!("materialise the project lead's persona: {e}"))?;
        let now = chrono::Utc::now();
        self.agents
            .create(&baybo_store::AgentProfileRow {
                id,
                description: LEAD_DESCRIPTION.to_owned(),
                avatar_blob_id: None,
                framework: AgentFramework::Baybo,
                llm: baybo_model::LlmPin::unpinned(),
                builtin: false,
                team: Some(TeamMembership {
                    project_id: project.clone(),
                    handle,
                }),
                // Nobody hired the lead: it comes with the board.
                hired_by: None,
                deleted_at: None,
                created_at: now,
                updated_at: now,
            })
            .await?;
        Ok(())
    }

    /// One project's live team, by handle. The lead is always in it.
    pub async fn team(&self, project: &ProjectId) -> Result<Vec<baybo_store::AgentProfileRow>> {
        self.get_project(project).await?;
        Ok(self.agents.list_team(project).await?)
    }

    /// The profile rows for `ids` **on this board**, reaching agents that
    /// have left its team.
    pub(crate) async fn agent_profiles(
        &self,
        project: &ProjectId,
        ids: impl IntoIterator<Item = AgentProfileId>,
    ) -> Vec<baybo_store::AgentProfileRow> {
        crate::actors::profiles(&self.agents, project, ids).await
    }

    /// Put somebody new on a team.
    pub async fn hire(
        &self,
        project: &ProjectId,
        new: NewTeamMember,
        hired_by: Option<AgentProfileId>,
    ) -> Result<baybo_store::AgentProfileRow> {
        self.writable_project(project).await?;
        let name = validate_agent_name(&new.name)?;
        let role = validate_role(&new.role)?;
        let team = self.agents.list_team(project).await?;
        if team.len() >= MAX_TEAM_AGENTS {
            return Err(ProjectError::invalid(
                "team",
                format!(
                    "this project already has {MAX_TEAM_AGENTS} agents. Remove one before \
                     adding another."
                ),
            ));
        }
        // `validate_agent_name` already refused anything this could reject.
        let base =
            AgentHandle::parse(&name).map_err(|e| ProjectError::invalid("name", e.reason))?;

        let id = AgentProfileId::generate_project();
        let soul =
            baybo_workspace::prompt::PROJECT_TEAMMATE_SOUL_TEMPLATE.replace("{{role}}", &role);
        baybo_workspace::ensure_named_persona_layout(&self.paths, id.as_str(), &soul, &name)
            .await
            .map_err(|e| anyhow::anyhow!("materialise the new agent's persona: {e}"))?;

        let now = chrono::Utc::now();
        let row = baybo_store::AgentProfileRow {
            id,
            description: role,
            avatar_blob_id: None,
            framework: new.framework.unwrap_or_default(),
            llm: new.llm,
            builtin: false,
            team: Some(TeamMembership {
                project_id: project.clone(),
                handle: base.clone(),
            }),
            hired_by,
            deleted_at: None,
            created_at: now,
            updated_at: now,
        };
        for attempt in 1..=MAX_HANDLE_ATTEMPTS {
            let candidate = if attempt == 1 {
                base.clone()
            } else {
                suffixed_handle(&base, attempt)?
            };
            let row = baybo_store::AgentProfileRow {
                team: Some(TeamMembership {
                    project_id: project.clone(),
                    handle: candidate,
                }),
                ..row.clone()
            };
            match self.agents.create(&row).await {
                Ok(()) => {
                    self.events.project_changed(project);
                    return Ok(row);
                }
                Err(baybo_store::StorageError::Conflict(_)) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(ProjectError::invalid(
            "name",
            format!(
                "@{base} and every numbered variant are taken on this board. Handles stay \
                 reserved after an agent is removed, so pick a different name."
            ),
        ))
    }

    /// Take somebody off a team.
    pub async fn remove_from_team(
        &self,
        project: &ProjectId,
        agent: &AgentProfileId,
    ) -> Result<()> {
        self.writable_project(project).await?;
        let profile = self
            .agents
            .get(agent)
            .await?
            .filter(|p| {
                p.team
                    .as_ref()
                    .is_some_and(|team| &team.project_id == project)
            })
            .ok_or_else(|| {
                ProjectError::invalid("agent", format!("{agent} is not on this project's team"))
            })?;
        if profile
            .team
            .as_ref()
            .is_some_and(|team| team.handle.as_str() == LEAD_HANDLE)
        {
            return Err(ProjectError::invalid(
                "agent",
                "the lead coordinates the board and cannot be removed from it",
            ));
        }
        let busy = self
            .store
            .active_runs(project)
            .await?
            .into_iter()
            .any(|run| &run.agent_id == agent);
        if busy {
            return Err(ProjectError::invalid(
                "agent",
                format!("{agent} has a run in flight — cancel it before removing the agent"),
            ));
        }
        if !self.agents.remove_from_team(agent).await? {
            // Somebody removed it between the read and the write.
            return Err(ProjectError::invalid(
                "agent",
                format!("{agent} is no longer on this project's team"),
            ));
        }
        self.events.project_changed(project);
        Ok(())
    }

    async fn materialise_workdir(&self, name: &str) -> Result<String> {
        let slug = slugify(name);
        let dir = self.paths.work_dir().join(&slug);
        if dir.exists() {
            let mut entries = tokio::fs::read_dir(&dir)
                .await
                .map_err(|e| anyhow::anyhow!("read {}: {e}", dir.display()))?;
            let occupied = entries
                .next_entry()
                .await
                .map_err(|e| anyhow::anyhow!("read {}: {e}", dir.display()))?
                .is_some();
            if occupied {
                return Err(ProjectError::invalid(
                    "workdir",
                    format!(
                        "{} already exists and is not empty. Point the project at it \
                         explicitly, or rename the project.",
                        dir.display()
                    ),
                ));
            }
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| anyhow::anyhow!("create {}: {e}", dir.display()))?;
        git_init(&dir).await?;
        Ok(dir.to_string_lossy().into_owned())
    }

    pub async fn list_projects(&self, include_archived: bool) -> Result<Vec<ProjectRow>> {
        Ok(self.store.list_projects(include_archived).await?)
    }

    pub async fn get_project(&self, id: &ProjectId) -> Result<ProjectRow> {
        self.store
            .get_project(id)
            .await?
            .ok_or_else(|| ProjectError::NoSuchProject(id.clone()))
    }

    pub async fn update_project(
        &self,
        id: &ProjectId,
        update: ProjectUpdate,
    ) -> Result<ProjectRow> {
        let before = self.writable_project(id).await?;
        validate_ceilings(update.daily_budget, update.daily_budget_tokens)?;
        let update = ProjectUpdate {
            name: validate_name(&update.name)?,
            description: update.description.trim().to_owned(),
            daily_budget: update.daily_budget,
            daily_budget_tokens: update.daily_budget_tokens,
            max_parallel_issue_runs: update.max_parallel_issue_runs,
            agents_may_merge: update.agents_may_merge,
        };
        // A ceiling is the one field here that can stop a board, and it left
        // no trace anywhere: no event, no timeline row, and `updated_at` is
        // bumped by any field, so "when did this board get a 100k/day token
        // ceiling, and was it meant" had no answer at all. This is not an
        // audit trail — a board-level event stream is what that would take,
        // and none exists — but it does put the before and after somewhere a
        // later investigation can read.
        if before.daily_budget != update.daily_budget
            || before.daily_budget_tokens != update.daily_budget_tokens
        {
            tracing::info!(
                project = %id,
                money_from = ?before.daily_budget,
                money_to = ?update.daily_budget,
                tokens_from = ?before.daily_budget_tokens,
                tokens_to = ?update.daily_budget_tokens,
                "a daily ceiling changed"
            );
        }
        // Same reasoning, and a stronger case for it: this one decides
        // whether the board may write its own trunk.
        if before.agents_may_merge != update.agents_may_merge {
            tracing::info!(
                project = %id,
                from = before.agents_may_merge,
                to = update.agents_may_merge,
                "whether this board's agents may merge changed"
            );
        }
        self.store.update_project(id, &update).await?;
        self.events.project_changed(id);
        // A raised ceiling should start the work it was blocking, without
        // the operator having to touch each card.
        if let Err(e) = self.release_held_runs(id).await {
            tracing::error!(project = %id, error = %e, "could not release held runs after a budget change");
        }
        self.get_project(id).await
    }

    /// Archive or restore. Either is allowed on a board already in that
    /// state — a repeat is not an error, it is simply nothing.
    pub async fn set_project_archived(&self, id: &ProjectId, archived: bool) -> Result<ProjectRow> {
        // `Ok(false)` is "nothing moved" — a board already in that state,
        // which owes nothing, or no board at all, which is an error. The
        // read this method ends in anyway is what tells the two apart.
        if !self.store.set_project_archived(id, archived).await? {
            return self.get_project(id).await;
        }
        self.events.project_changed(id);
        if !archived && let Err(e) = self.resume_project_runs(id).await {
            tracing::error!(project = %id, error = %e, "could not resume a board's runs after a restore");
        }
        self.get_project(id).await
    }

    pub async fn list_issues(&self, project: &ProjectId) -> Result<Vec<IssueRow>> {
        // Resolve the parent first: an unknown board and an empty board are
        // different answers, and only this call can tell them apart.
        self.get_project(project).await?;
        Ok(self.store.list_issues(project).await?)
    }

    pub async fn get_issue(&self, project: &ProjectId, number: i64) -> Result<IssueRow> {
        self.get_project(project).await?;
        self.store
            .get_issue(project, number)
            .await?
            .ok_or_else(|| ProjectError::NoSuchIssue {
                project: project.clone(),
                number,
            })
    }

    /// The card an approval may be answered on.
    ///
    /// Answering releases an agent to act, which makes it a write in the
    /// only sense this gate measures, so it goes through the same archived
    /// check every other write does. An archived board's parked prompt is
    /// left to time out — the alternative is a board that takes no comments
    /// and no moves yet can still be talked into running a command.
    ///
    /// Two consequences worth stating, because neither is visible from the
    /// call site. Archiving does not stop a run that is already executing
    /// (only `promotions` reads the flag), so a run that reaches a gate
    /// just after the operator archives its board now waits out
    /// `APPROVAL_TIMEOUT` and self-denies rather than being answered
    /// either way — the same end state a denial reaches, later. And this
    /// gate covers the REST door only: `Frame::ResolveApproval` resolves a
    /// bare `call_id` against the same queue with no card and no board in
    /// hand (`gateway/src/channel/route.rs`), so a client subscribed to an
    /// issue's session could still answer one there. No shipped client
    /// does — issue sessions are excluded from every chat surface — but
    /// this is a gate with two doors and only one of them checks.
    pub async fn approvable_issue(&self, project: &ProjectId, number: i64) -> Result<IssueRow> {
        self.writable_project(project).await?;
        self.get_issue(project, number).await
    }

    pub async fn create_issue(
        &self,
        project: &ProjectId,
        actor: IssueActor,
        new: NewIssueRequest,
    ) -> Result<Opened> {
        self.writable_project(project).await?;
        let title = validate_issue_title(&new.title)?;
        let attachments = attachments::resolve(&self.blobs, &new.attachments).await?;
        if let Some(assignee) = new.assignee.as_ref() {
            self.validate_assignee(project, assignee).await?;
        }
        self.validate_staffing(new.status, new.assignee.as_ref())?;
        let parent = match new.parent {
            Some(number) => Some(self.validate_parent(project, number, None).await?),
            None => None,
        };
        let filed_from = match new.filed_from.as_ref() {
            Some(id) => Some(self.issue_by_id(project, id, "filed_from").await?),
            None => None,
        };
        if let Some(key) = new.source_key.as_deref()
            && let Some(standing) = self.store.live_issue_by_source_key(project, key).await?
        {
            return Ok(Opened::AlreadyOpen(standing));
        }
        let issue = self
            .store
            .create_issue(&NewIssue {
                id: IssueId::generate(),
                project_id: project.clone(),
                title,
                description: new.description.trim().to_owned(),
                attachments,
                status: new.status,
                priority: new.priority,
                assignee: new.assignee,
                parent_issue_id: parent.as_ref().map(|p| p.id.clone()),
                stage: if parent.is_some() { new.stage } else { 0 },
                source_key: new.source_key,
                filed_from: filed_from.as_ref().map(|origin| origin.id.clone()),
                created_at: chrono::Utc::now(),
            })
            .await
            .map_err(|e| match e {
                // The index won a race with the check above. Reporting the
                // standing card is the same answer the check would have
                // given a moment earlier.
                baybo_store::StorageError::Conflict(reason) => ProjectError::Conflict(reason),
                other => other.into(),
            })?;
        self.events.board_changed(project, Some(issue.number));
        self.record(&issue, actor.clone(), IssueEventBody::Opened)
            .await;
        if let Some(origin) = filed_from.as_ref() {
            self.record(
                origin,
                actor.clone(),
                IssueEventBody::Filed {
                    number: issue.number,
                },
            )
            .await;
        }
        if let Some(assignee) = issue.assignee.clone() {
            self.record(
                &issue,
                actor.clone(),
                IssueEventBody::Assigned {
                    from: None,
                    to: Some(assignee),
                },
            )
            .await;
        }
        let _ = self
            .dispatch_if_triggered(Transition::created(&issue), &issue, &actor)
            .await;
        Ok(Opened::Created(issue))
    }

    /// The `parent`/`stage` half of a patch, as a wire caller expresses it.
    ///
    /// `parent` is an issue number and `0` is not one, so `0` used to be
    /// spare capacity the detach was hung on. That put the detach behind a
    /// **filler value**: a model answering a strict tool schema fills every
    /// field it is offered, so a run reporting that it could not start named
    /// `parent` and `stage` alongside its block reason — filling both with
    /// `0` — and took the card out of its parent's plan on the way to saying
    /// something else entirely. Detaching is [`Placement::detach`] now,
    /// which no filler produces.
    ///
    /// `stage` rides with `parent` for the same reason and one more: the two
    /// are **one fact** — where this card sits under that parent — and half
    /// of it is not a placement. A patch naming a stage and no parent is not
    /// asking for anything this can answer.
    pub async fn resolve_placement(
        &self,
        project: &ProjectId,
        placement: Placement,
    ) -> Result<(Option<Option<IssueId>>, Option<i64>)> {
        if placement.detach {
            // A stage number under a parent that is no longer there is a
            // barrier the next reader would honour.
            return Ok((Some(None), Some(0)));
        }
        let Some(number) = placement.parent.filter(|number| *number > 0) else {
            return Ok((None, None));
        };
        let parent = self.get_issue(project, number).await?;
        Ok((Some(Some(parent.id)), placement.stage))
    }

    /// Edit a card. `attachments` replaces the description's file list
    /// wholesale — `Some(&[])` clears it, `None` leaves it alone.
    ///
    /// It is a parameter rather than a field on `update` because a caller
    /// may not name a stored attachment: only blob ids go in, and what
    /// comes out is whatever [`crate::attachments::resolve`] could vouch
    /// for. `update.attachments` is this method's own output and is
    /// overwritten.
    pub async fn update_issue(
        &self,
        project: &ProjectId,
        number: i64,
        actor: IssueActor,
        update: IssueUpdate,
        attachments: Option<&[AttachmentRequest]>,
    ) -> Result<IssueRow> {
        self.writable_project(project).await?;
        let attachments = match attachments {
            Some(requested) => Some(attachments::resolve(&self.blobs, requested).await?),
            None => None,
        };
        if update.is_empty() && attachments.is_none() {
            return Err(ProjectError::invalid("update", "sets no field"));
        }
        if let Some(Some(parent_id)) = update.parent.as_ref() {
            let parent = self.issue_by_id(project, parent_id, "parent").await?;
            let child = self.get_issue(project, number).await?;
            self.validate_parent_row(&parent, Some(&child)).await?;
        }
        if let Some(next) = update.assignee.as_ref() {
            if let Some(assignee) = next.as_ref() {
                self.validate_assignee(project, assignee).await?;
            }
            // Checked against the column the issue is actually in: dropping
            // the assignee of in-flight work recreates exactly the zombie
            // the staffing rule exists to prevent.
            let current = self.get_issue(project, number).await?;
            self.validate_staffing(current.status, next.as_ref())?;
        }
        // Refused here and not in the tool: the REST door writes through this
        // function too, and a rule spelled at one of two doors is the rule
        // drifting. The operator's own reopen is `IssueActor::User` and is
        // untouched.
        if update.cancelled == Some(false) && matches!(actor, IssueActor::Agent(_)) {
            let standing = self.get_issue(project, number).await?;
            if standing.cancelled_at.is_some() && self.stopped_by_a_person(&standing).await {
                return Err(ProjectError::invalid(
                    "cancelled",
                    AGENT_REOPENING_A_PERSONS_STOP,
                ));
            }
        }
        let update = IssueUpdate {
            title: update
                .title
                .as_deref()
                .map(validate_issue_title)
                .transpose()?,
            description: update.description.map(|d| d.trim().to_owned()),
            attachments,
            blocked_reason: update.blocked_reason.map(|reason| {
                reason.and_then(|r| {
                    let trimmed = r.trim();
                    // An all-whitespace reason is an unblock, not a block
                    // with a blank explanation.
                    (!trimmed.is_empty()).then(|| trimmed.to_owned())
                })
            }),
            ..update
        };
        let before = self.get_issue(project, number).await?;
        if !self.store.update_issue(project, number, &update).await? {
            return Err(ProjectError::NoSuchIssue {
                project: project.clone(),
                number,
            });
        }
        let after = self.get_issue(project, number).await?;
        self.events.board_changed(project, Some(number));
        self.record_diff(&before, &after, actor.clone()).await;
        self.reclaim_if_finished(&before, &after, actor.clone())
            .await;
        let started = self
            .dispatch_if_triggered(Transition::between(&before, &after), &after, &actor)
            .await;
        self.check_stage_barrier(&before, &after, actor).await;
        if before.blocked_reason.is_some() && after.blocked_reason.is_none() {
            self.redrive_after_unblock(&after, started.as_ref()).await;
        }
        Ok(after)
    }

    async fn check_stage_barrier(&self, before: &IssueRow, after: &IssueRow, actor: IssueActor) {
        if crate::stages::is_finished(before) || !crate::stages::is_finished(after) {
            return;
        }
        let Some(parent_id) = after.parent_issue_id.clone() else {
            return;
        };
        let children = match self.store.list_children(&parent_id).await {
            Ok(children) => children,
            Err(e) => {
                tracing::error!(issue = after.number, error = %e, "could not read a parent's children");
                return;
            }
        };
        if !crate::stages::stage_complete(&children, after.stage) {
            return;
        }
        // The parent is addressed by number like everything else, so the
        // barrier reads it back rather than carrying a half-row around.
        let parent = match self.store.list_issues(&after.project_id).await {
            Ok(issues) => issues.into_iter().find(|issue| issue.id == parent_id),
            Err(e) => {
                tracing::error!(issue = after.number, error = %e, "could not read the parent issue");
                return;
            }
        };
        let Some(parent) = parent else {
            return;
        };
        self.record(
            &parent,
            actor.clone(),
            IssueEventBody::StageCompleted { stage: after.stage },
        )
        .await;
        // Three gates and one reason for all of them: the barrier opening is
        // the board acting on its own, and none of the three is its to
        // override. Nobody on the parent means nobody to wake; a block is a
        // decision somebody recorded; a parent in Backlog is that same
        // decision spelled as a column, and the column is the gate that was
        // missing — `enqueue` never reads the parallelism ceiling either, so
        // neither of the operator's two ways to stop a board held this door.
        // The `StageCompleted` entry above lands either way, so whoever
        // staffs, unblocks or un-parks it sees the stage opened.
        if parent.assignee.is_some()
            && crate::driver::is_live_work(parent.status)
            && crate::driver::board_may_start(&parent)
            && crate::stages::barrier_opens(&children, after.stage)
        {
            self.enqueue(&parent, RunTrigger::StageBarrier, &actor)
                .await;
        }
    }

    /// Move an issue into `status`, with `ordered_numbers` giving that
    /// column's full contents in their new order.
    pub async fn move_issue(
        &self,
        project: &ProjectId,
        number: i64,
        actor: IssueActor,
        status: IssueStatus,
        ordered_numbers: &[i64],
    ) -> Result<IssueRow> {
        self.writable_project(project).await?;
        let before = self.get_issue(project, number).await?;
        self.validate_column_order(project, number, status, ordered_numbers)
            .await?;
        self.validate_staffing(status, before.assignee.as_ref())?;
        if !self
            .store
            .move_issue(project, number, status, ordered_numbers)
            .await?
        {
            return Err(ProjectError::NoSuchIssue {
                project: project.clone(),
                number,
            });
        }
        let after = self.get_issue(project, number).await?;
        self.record_diff(&before, &after, actor.clone()).await;
        self.reclaim_if_finished(&before, &after, actor.clone())
            .await;
        let _ = self
            .dispatch_if_triggered(Transition::between(&before, &after), &after, &actor)
            .await;
        self.check_stage_barrier(&before, &after, actor).await;
        Ok(after)
    }

    /// One board's capacity right now: who is actually executing, what is
    /// parked on budget, and how much is left to spend.
    pub(crate) async fn board_load(&self, project: &ProjectId) -> Result<BoardLoad> {
        self.get_project(project).await?;
        let (held, working) = self
            .store
            .active_runs(project)
            .await?
            .into_iter()
            .partition(|run| run.status == RunStatus::Held);
        Ok(BoardLoad {
            headroom: self.headroom(project).await,
            working,
            held,
        })
    }

    /// What is stuck on the operator, per board.
    /// What every board is doing right now, keyed by project. `since` is
    /// the start of the operator's day — passed in rather than computed
    /// here, because "today" is a question about the caller's clock and
    /// this crate has no business guessing their timezone.
    pub async fn board_activity(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<(ProjectId, baybo_store::project::BoardActivity)>> {
        Ok(self.store.board_activity(since).await?)
    }

    pub async fn attention(
        &self,
        pending_approval_sessions: &[baybo_model::SessionId],
    ) -> Result<Vec<(ProjectId, baybo_store::project::AttentionCounts)>> {
        let mut counts: std::collections::HashMap<
            ProjectId,
            baybo_store::project::AttentionCounts,
        > = self.store.attention().await?.into_iter().collect();
        for (_, project) in self
            .store
            .projects_for_sessions(pending_approval_sessions)
            .await?
        {
            // Only boards the query above already considered: it excludes
            // archived ones, and a prompt parked on an archived board's old
            // run is not work anybody is being asked to do.
            if let Some(entry) = counts.get_mut(&project) {
                entry.approvals += 1;
            } else if self
                .store
                .get_project(&project)
                .await?
                .is_some_and(|row| row.archived_at.is_none())
            {
                counts.entry(project).or_default().approvals += 1;
            }
        }
        // No empty-row filter: every group SQL emits has at least one row,
        // and the approval pass only ever increments — so a board with
        // nothing waiting is absent rather than a row of zeroes.
        let mut out: Vec<_> = counts.into_iter().collect();
        // Stable order so a badge does not reshuffle between polls.
        out.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        Ok(out)
    }

    /// Note that the operator has opened this card.
    ///
    /// Per card, never per board: an operator who reads the question asked
    /// on #3 has not read the one asked on #7, and a board-wide stamp is
    /// how the badge used to swallow both.
    pub async fn mark_issue_read(&self, project: &ProjectId, number: i64) -> Result<()> {
        let issue = self.get_issue(project, number).await?;
        self.store
            .mark_issue_read(&issue.id, chrono::Utc::now())
            .await?;
        self.events.timeline_changed(project, number);
        Ok(())
    }

    /// Note that the operator has read the whole board, in one press.
    ///
    /// This does not put back the per-board cursor the per-card one
    /// replaced. That one was **automatic** — fired by the board page's load
    /// effect, so merely arriving swallowed every question asked while the
    /// page was still fetching. This one is an act: the operator says they
    /// have seen it, on a board they are looking at, and it moves the same
    /// per-card cursors nothing else moves.
    ///
    /// Archived boards take it, like [`Self::mark_issue_read`] and for the
    /// same reason — noting that something was seen is not an addition to
    /// the board.
    pub async fn mark_project_read(&self, project: &ProjectId) -> Result<()> {
        self.get_project(project).await?;
        self.store
            .mark_project_read(project, chrono::Utc::now())
            .await?;
        // Every card's face may have changed, so this is the board-wide
        // frame rather than one card's timeline.
        self.events.board_changed(project, None);
        Ok(())
    }

    /// The board's cards with every card's signals resolved.
    ///
    /// The one door for anything that draws a card face. Callers get the
    /// resolved [`CardSignals`](baybo_store::project::CardSignals) rather
    /// than the rows plus a recipe: "this card's newest run failed" is a
    /// rule with one home, and a caller holding the runs would answer it a
    /// second time and differently.
    pub async fn board_cards(&self, project: &ProjectId) -> Result<BoardCards> {
        let rows = self.list_issues(project).await?;
        let signals = self.store.card_signals(project).await?;
        let opened_by_agent = self
            .store
            .agent_opened_issues(project)
            .await?
            .into_iter()
            .collect();
        Ok(BoardCards::new(rows, signals, opened_by_agent))
    }

    /// The whole board's activity, newest first. Two sources merged here
    /// rather than in the caller: a card's timeline lives in
    /// `issue_events`, but joining the team is a fact about the *board*
    /// with no card to hang a row on, and `agent_profiles` already records
    /// who joined, when, and who hired them. Writing it a second time as an
    /// event would be the cron-groups mistake — a derived thing stored, and
    /// then free to disagree with the roster it was derived from.
    pub async fn feed(
        &self,
        project: &ProjectId,
        before: Option<chrono::DateTime<chrono::Utc>>,
        limit: usize,
    ) -> Result<Vec<FeedEntry>> {
        self.get_project(project).await?;
        let limit = limit.clamp(1, MAX_FEED_PAGE);
        let cards = self.store.project_feed(project, before, limit).await?;
        // Tombstones included: the feed is a record, and a hire that
        // disappeared when its agent was removed would be the history
        // rewriting itself.
        let hires = self.agents.list_team_history(project).await?;

        // One batched derivation for the whole page. A page names runs on a
        // dozen different cards, so asking per card would turn one screen
        // into a dozen round trips.
        let settled: Vec<IssueRunId> = cards
            .iter()
            .filter_map(|row| match &row.body {
                IssueEventBody::RunSettled { run_id, .. } => Some(run_id.clone()),
                _ => None,
            })
            .collect();
        let mut facts: HashMap<IssueRunId, SettledRun> = self
            .store
            .settled_run_facts(&settled)
            .await?
            .into_iter()
            .map(|one| {
                (
                    one.run_id,
                    SettledRun {
                        duration_ms: one.duration_ms,
                        cost_micros: one.spend.cost.into_micros(),
                    },
                )
            })
            .collect();

        let mut merged: Vec<FeedEntry> = cards
            .into_iter()
            .map(|row| {
                let settled = match &row.body {
                    IssueEventBody::RunSettled { run_id, .. } => facts.remove(run_id),
                    _ => None,
                };
                FeedEntry::Card { row, settled }
            })
            .collect();
        merged.extend(
            hires
                .into_iter()
                .filter(|row| before.is_none_or(|cutoff| row.created_at < cutoff))
                // The lead is not a hire: it is seeded with the board, so
                // "hired @lead" would be the oldest line on every feed and
                // would say nothing the board's existence does not.
                .filter(|row| {
                    row.team
                        .as_ref()
                        .is_none_or(|team| team.handle.as_str() != LEAD_HANDLE)
                })
                .map(|row| FeedEntry::Hired {
                    agent: row.id,
                    hired_by: row.hired_by,
                    at: row.created_at,
                }),
        );
        merged.sort_by_key(|entry| std::cmp::Reverse(entry.at()));
        merged.truncate(limit);
        Ok(merged)
    }

    /// One issue's sub-issues, by stage then position.
    pub async fn children(&self, project: &ProjectId, number: i64) -> Result<Vec<IssueRow>> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.store.list_children(&issue.id).await?)
    }

    async fn validate_parent(
        &self,
        project: &ProjectId,
        parent_number: i64,
        child: Option<&IssueRow>,
    ) -> Result<IssueRow> {
        let parent = self.get_issue(project, parent_number).await?;
        self.validate_parent_row(&parent, child).await?;
        Ok(parent)
    }

    async fn validate_parent_row(&self, parent: &IssueRow, child: Option<&IssueRow>) -> Result<()> {
        if let Some(child) = child {
            if child.id == parent.id {
                return Err(ProjectError::invalid(
                    "parent",
                    "an issue cannot be a step of itself",
                ));
            }
            if !self.store.list_children(&child.id).await?.is_empty() {
                return Err(ProjectError::invalid(
                    "parent",
                    format!(
                        "#{} has sub-issues of its own, and sub-issues are one level deep",
                        child.number
                    ),
                ));
            }
        }
        if parent.parent_issue_id.is_some() {
            return Err(ProjectError::invalid(
                "parent",
                format!(
                    "#{} is already a sub-issue, and sub-issues are one level deep",
                    parent.number
                ),
            ));
        }
        Ok(())
    }

    /// This board's issue with that ULID. `field` names the caller's input
    /// in the 400 a miss produces, so a bad parent and a bad origin do not
    /// both report themselves as a bad parent.
    pub async fn issue_by_id(
        &self,
        project: &ProjectId,
        id: &IssueId,
        field: &'static str,
    ) -> Result<IssueRow> {
        self.store
            .list_issues(project)
            .await?
            .into_iter()
            .find(|issue| &issue.id == id)
            .ok_or_else(|| ProjectError::invalid(field, format!("no issue {id} on this board")))
    }

    async fn validate_column_order(
        &self,
        project: &ProjectId,
        number: i64,
        status: IssueStatus,
        ordered_numbers: &[i64],
    ) -> Result<()> {
        let mut expected: std::collections::BTreeSet<i64> = self
            .store
            .list_issues(project)
            .await?
            .into_iter()
            .filter(|issue| issue.status == status && issue.number != number)
            .map(|issue| issue.number)
            .collect();
        // The moved card belongs in the destination whether or not it is
        // there yet, which is what makes one rule cover both a reorder and a
        // cross-column move.
        expected.insert(number);
        let given: std::collections::BTreeSet<i64> = ordered_numbers.iter().copied().collect();
        if given == expected {
            return Ok(());
        }
        let missing: Vec<i64> = expected.difference(&given).copied().collect();
        let extra: Vec<i64> = given.difference(&expected).copied().collect();
        Err(ProjectError::invalid(
            "ordered_numbers",
            format!(
                "must be {}'s whole new contents, in order. Missing: {missing:?}. \
                 Not in that column: {extra:?}.",
                status.as_str()
            ),
        ))
    }

    async fn validate_assignee(
        &self,
        project: &ProjectId,
        assignee: &AgentProfileId,
    ) -> Result<()> {
        let profile = self
            .agents
            .get(assignee)
            .await?
            .ok_or_else(|| ProjectError::invalid("assignee", format!("no agent {assignee}")))?;
        let on_this_board = profile
            .team
            .as_ref()
            .is_some_and(|team| &team.project_id == project);
        if !on_this_board {
            return Err(ProjectError::invalid(
                "assignee",
                format!("{assignee} is not on this project's team"),
            ));
        }
        if profile.deleted_at.is_some() {
            return Err(ProjectError::invalid(
                "assignee",
                format!("{assignee} was removed from this project's team"),
            ));
        }
        if !crate::runs::can_host_a_session(profile.framework) {
            return Err(ProjectError::invalid(
                "assignee",
                framework_refusal(assignee, profile.framework),
            ));
        }
        Ok(())
    }

    /// Whether this agent can host a run at all. `None` is "the store could
    /// not say", which every caller treats as "do not start anything" —
    /// distinct from `Some(false)`, which is a settled no.
    async fn can_run(&self, agent: &AgentProfileId) -> Option<bool> {
        match self.agents.get(agent).await {
            Ok(Some(profile)) => Some(crate::runs::can_host_a_session(profile.framework)),
            Ok(None) => Some(false),
            Err(e) => {
                tracing::error!(%agent, error = %e, "could not read an agent's framework");
                None
            }
        }
    }

    fn validate_staffing(
        &self,
        status: IssueStatus,
        assignee: Option<&AgentProfileId>,
    ) -> Result<()> {
        if status == IssueStatus::InProgress && assignee.is_none() {
            return Err(ProjectError::invalid(
                "assignee",
                "In Progress needs an assignee — assign someone before starting the work",
            ));
        }
        Ok(())
    }

    /// Land an issue's branch in the repository's own checkout.
    ///
    /// The one door. Every question a merge has to answer — may this board
    /// merge at all, has the card been looked at, which branch is the card's,
    /// is either tree dirty — is answered here rather than at the tool, so
    /// that a second caller cannot answer any of them differently. The git
    /// itself lives in [`crate::worktree::merge`], beside every other verb
    /// that touches a card's checkout.
    ///
    /// A refusal is a value, not an error: the caller is an agent, and
    /// "your board does not do this" is an answer it can act on, while an
    /// `Err` reads as a fault it should retry.
    pub async fn merge_issue_branch(
        &self,
        project: &ProjectId,
        number: i64,
        actor: IssueActor,
    ) -> Result<crate::worktree::Merged> {
        let row = self.writable_project(project).await?;
        if !row.agents_may_merge {
            return Ok(crate::worktree::Merged::Refused {
                reason: "this board's agents do not merge. Its branch is the artefact it hands \
                         over; say on the card that the work is ready and let the operator land \
                         it."
                .to_owned(),
                retryable: false,
            });
        }
        let issue = self.get_issue(project, number).await?;
        if !MERGEABLE_FROM.contains(&issue.status) {
            return Ok(crate::worktree::Merged::Refused {
                reason: format!(
                    "#{number} is in {}, and a branch is merged once its work has been looked \
                     at — move it to review first.",
                    issue.status.as_str()
                ),
                retryable: false,
            });
        }
        if issue.cancelled_at.is_some() {
            return Ok(crate::worktree::Merged::Refused {
                reason: format!("#{number} is cancelled, so its branch is not work to land"),
                retryable: false,
            });
        }
        let root = crate::worktree::worktree_root(&self.paths, project, number);
        // The card's own record wins where it has one. `branch_worked_on`
        // falls back to re-deriving the name from the *current* title once
        // the checkout is gone — and a Done card's checkout is always gone,
        // because reclamation runs on the way in. A retitle between the
        // branch being recorded and the merge being asked for would
        // otherwise send this at a ref that never existed.
        let branch = match issue.branch.as_deref() {
            Some(recorded) => recorded.to_owned(),
            None => self.branch_worked_on(&root, &issue).await,
        };
        // Recorded *before* the merge, not after: a merged branch is zero
        // commits ahead, and `record_branch`'s guard would then refuse to
        // record it for the rest of the card's life. The card would end up
        // naming no branch at all, right after landing one.
        if issue.branch.is_none()
            && let Ok(true) = self.store.set_issue_branch(&issue.id, &branch).await
        {
            self.events.board_changed(project, Some(number));
        }

        let merged = {
            let _one_at_a_time = self.merging.lock().await;
            crate::worktree::merge(
                Path::new(&row.workdir),
                &root,
                &branch,
                &format!("Merge #{number}: {}", issue.title),
            )
            .await?
        };

        if let crate::worktree::Merged::Landed {
            into,
            commit,
            commits,
        } = &merged
        {
            self.store
                .append_event(&NewIssueEvent {
                    issue_id: issue.id.clone(),
                    project_id: project.clone(),
                    number,
                    actor,
                    body: IssueEventBody::BranchMerged {
                        branch: branch.clone(),
                        into: into.clone(),
                        commit: commit.clone(),
                        commits: *commits,
                    },
                })
                .await?;
            self.events.timeline_changed(project, number);
            self.events.board_changed(project, Some(number));
        }
        Ok(merged)
    }

    async fn writable_project(&self, id: &ProjectId) -> Result<ProjectRow> {
        let project = self.get_project(id).await?;
        if project.archived_at.is_some() {
            return Err(ProjectError::Archived(id.clone()));
        }
        Ok(project)
    }
}

fn validate_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ProjectError::invalid("name", "must not be empty"));
    }
    if trimmed.chars().count() > MAX_PROJECT_NAME_CHARS {
        return Err(ProjectError::invalid(
            "name",
            format!("longer than {MAX_PROJECT_NAME_CHARS} characters"),
        ));
    }
    Ok(trimmed.to_owned())
}

const FIELD_DAILY_BUDGET: &str = "daily_budget";
const FIELD_DAILY_BUDGET_TOKENS: &str = "daily_budget_tokens";

const CEILING_REFUSAL: &str =
    "must not be negative — use 0 to pause the board, or leave it unset for no limit";

fn validate_ceiling(field: &'static str, ceiling: Option<i64>) -> Result<()> {
    if ceiling.is_some_and(|c| c < 0) {
        return Err(ProjectError::invalid(field, CEILING_REFUSAL));
    }
    Ok(())
}

fn validate_ceilings(
    daily_budget: Option<baybo_model::MicroUsd>,
    daily_budget_tokens: Option<i64>,
) -> Result<()> {
    validate_ceiling(
        FIELD_DAILY_BUDGET,
        daily_budget.map(baybo_model::MicroUsd::into_micros),
    )?;
    validate_ceiling(FIELD_DAILY_BUDGET_TOKENS, daily_budget_tokens)
}

/// An agent's name **is** its handle.
///
/// It used to be a display name that a handle was derived from — "Test
/// Engineer" becoming `@test-engineer`. Two names for one teammate bought
/// nothing: every surface on the board addresses agents by handle, so the
/// display name only ever appeared beside the handle it produced, and the
/// operator had to be shown a preview of a slug they did not type to know
/// what they were about to be stuck with.
///
/// Enforced here rather than in the form because this is the door both
/// hirings come through — the operator's and the lead's own
/// `ProjectAgentCreate`. A rule kept in the form would be a rule the lead
/// does not have, and the roster would carry both spellings.
fn validate_agent_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    AgentHandle::parse(trimmed).map_err(|e| ProjectError::invalid("name", e.reason))?;
    Ok(trimmed.to_owned())
}

fn validate_role(role: &str) -> Result<String> {
    let trimmed = role.trim();
    if trimmed.is_empty() {
        return Err(ProjectError::invalid(
            "role",
            "say what this agent is for — it seeds the agent's own soul",
        ));
    }
    if trimmed.chars().count() > MAX_ROLE_CHARS {
        return Err(ProjectError::invalid(
            "role",
            format!("longer than {MAX_ROLE_CHARS} characters"),
        ));
    }
    Ok(trimmed.to_owned())
}

fn suffixed_handle(base: &AgentHandle, n: usize) -> Result<AgentHandle> {
    let suffix = format!("-{n}");
    let room = baybo_model::MAX_AGENT_HANDLE_CHARS.saturating_sub(suffix.chars().count());
    let stem: String = base.as_str().chars().take(room).collect();
    AgentHandle::parse(format!("{}{suffix}", stem.trim_end_matches('-'))).map_err(|e| {
        ProjectError::invalid("name", format!("could not number the handle @{base}: {e}"))
    })
}

fn validate_issue_title(title: &str) -> Result<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err(ProjectError::invalid("title", "must not be empty"));
    }
    if trimmed.chars().count() > MAX_ISSUE_TITLE_CHARS {
        return Err(ProjectError::invalid(
            "title",
            format!("longer than {MAX_ISSUE_TITLE_CHARS} characters"),
        ));
    }
    Ok(trimmed.to_owned())
}

/// A project name reduced to a directory name: lowercase, ASCII
/// alphanumerics and dashes, no runs, no leading or trailing dash.
pub(crate) fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.extend(ch.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() {
        // A name of pure punctuation still needs somewhere to live.
        format!("project-{}", ProjectId::generate().as_str().to_lowercase())
    } else {
        slug
    }
}

async fn git_init(dir: &PathBuf) -> Result<()> {
    let status = tokio::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(dir)
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("spawn `git init {}`: {e}", dir.display()))?;
    if !status.success() {
        return Err(ProjectError::Workdir(anyhow::anyhow!(
            "`git init {}` exited with {status}",
            dir.display()
        )));
    }
    Ok(())
}

/// Refuse a workdir that overlaps baybo's own workspace.
pub fn validate_workdir(workdir: &str, paths: &WorkspacePaths) -> Result<()> {
    let path = Path::new(workdir);
    if !path.is_absolute() {
        return Err(ProjectError::invalid("workdir", "must be an absolute path"));
    }
    let resolve = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let target = resolve(path);
    let root = resolve(&baybo_workspace::absolutise(paths.root()));
    let work = resolve(&baybo_workspace::absolutise(&paths.work_dir()));
    let inside = target.starts_with(&root) && !target.starts_with(&work);
    // No `work/` exemption on this side: containing `work/` does not stop a
    // parent from containing `state/` and `.key/` as well.
    let swallows = root.starts_with(&target);
    if inside || swallows {
        return Err(ProjectError::invalid(
            "workdir",
            format!(
                "{} overlaps baybo's own workspace at {}. A project's checkout is \
                 bound read-write into every shell command its team runs, so it must \
                 be neither inside that tree nor a parent of it (a checkout under {} \
                 is fine).",
                target.display(),
                root.display(),
                work.display(),
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_directory_safe() {
        assert_eq!(slugify("baybo"), "baybo");
        assert_eq!(slugify("My Project!"), "my-project");
        assert_eq!(slugify("  spaced  out  "), "spaced-out");
        assert_eq!(slugify("../escape"), "escape");
        assert_eq!(slugify("a//b"), "a-b");
        assert!(
            slugify("!!!").starts_with("project-"),
            "a name with nothing usable still gets a directory"
        );
    }
}
