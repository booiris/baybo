//! Project and issue lifecycle: validation, workdir materialisation, and
//! the board's write surface.
//!
//! The store below is a dumb writer. Everything that has to be true before
//! a row lands — a name that isn't blank, a workdir that doesn't overlap
//! baybo's own workspace, an issue that names a project that exists — is
//! decided here, once, so the HTTP layer and any future tool caller get
//! the same answers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use baybo_model::{
    AgentFramework, AgentHandle, AgentProfileId, IssueId, MAX_PROJECT_NAME_CHARS, ProjectId,
    TeamMembership,
};
use baybo_store::AgentProfileStore;
use baybo_store::project::{
    IssueActor, IssueEventBody, IssueEventRow, IssuePriority, IssueRow, IssueRunRow, IssueStatus,
    IssueUpdate, NewIssue, NewIssueEvent, ProjectRow, ProjectStore, ProjectUpdate, RunStatus,
    RunTrigger,
};
use baybo_workspace::WorkspacePaths;

use crate::error::{ProjectError, Result};
use crate::events::ProjectEvents;
use crate::runs::{Transition, ledger_entry, triggers_run};
use crate::{CommentDelivery, Headroom};

/// Upper bound on an issue title (chars, after trim). Long enough for a
/// sentence, short enough that a card face can show it.
pub const MAX_ISSUE_TITLE_CHARS: usize = 200;

/// The handle every project's coordinator answers to. Fixed rather than
/// derived: `@lead` means the same thing on every board, and it is the one
/// handle a person can type without looking the team up first.
pub const LEAD_HANDLE: &str = "lead";

/// What the lead calls itself before it renames itself.
const LEAD_DISPLAY_NAME: &str = "Lead";

/// The lead's roster line. The operator can edit it like any other agent's.
const LEAD_DESCRIPTION: &str =
    "Coordinates this project's board: triages Backlog, assigns work, and staffs the team.";

/// How many live agents one project may have, lead included.
///
/// A ceiling rather than a budget: each agent is a persona directory, a
/// slice of every roster read, and a possible concurrent run. The number is
/// not load-bearing — it exists so a hiring loop cannot staff a board with
/// two hundred agents before anybody notices.
pub const MAX_TEAM_AGENTS: usize = 16;

/// Upper bound on a teammate's role line (chars, after trim). It seeds a
/// SOUL and shows on a roster card, so it is a sentence, not a brief.
pub const MAX_ROLE_CHARS: usize = 280;

/// Upper bound on one activity-feed page.
pub const MAX_FEED_PAGE: usize = 100;

/// How many `-2`, `-3`, … suffixes to try when a derived handle is taken.
///
/// Bounded because handles stay reserved after removal: a board that has
/// hired and released "QA" a dozen times should get a clear refusal, not a
/// silent `@qa-47`.
const MAX_HANDLE_ATTEMPTS: usize = 9;

/// A board's capacity, as [`ProjectManager::board_load`] reports it.
#[derive(Debug, Clone)]
pub struct BoardLoad {
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
    /// Display name. The `@handle` is derived from it and then immutable.
    pub name: String,
    /// One line saying what this agent is for. Seeds its `SOUL.md` and
    /// becomes its roster description.
    pub role: String,
    /// `None` follows the workspace default (baybo). Only the operator's
    /// form sets this; `ProjectAgentCreate` deliberately does not.
    pub framework: Option<AgentFramework>,
    /// `None` follows `default-llm`.
    pub llm: Option<baybo_model::LlmEntryName>,
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
}

/// What a caller supplies to open an issue. Status is where the card
/// lands; there is no separate "column" concept.
#[derive(Debug, Clone)]
pub struct NewIssueRequest {
    pub title: String,
    pub description: String,
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
}

/// What happened when a card was opened.
///
/// An enum rather than a bool or a second method: a caller that passes no
/// `source_key` can never see `AlreadyOpen`, but the compiler still makes
/// every call site say what it does with it — which is the point, because
/// the two outcomes read identically in a log line.
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
    paths: WorkspacePaths,
    /// Where a board change is announced to whoever is watching it.
    events: Arc<dyn ProjectEvents>,
    /// Where a recorded run is announced. The ledger row is written first
    /// and this only nudges — a send that fails, or a receiver that died,
    /// costs a delay until the next boot sweep, never a lost run.
    dispatch: RunDispatch,
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

impl ProjectManager {
    pub fn new(
        store: Arc<dyn ProjectStore>,
        agents: Arc<dyn AgentProfileStore>,
        paths: WorkspacePaths,
        events: Arc<dyn ProjectEvents>,
        dispatch: RunDispatch,
    ) -> Self {
        Self {
            store,
            agents,
            paths,
            events,
            dispatch,
        }
    }

    /// Record a run if this transition starts one, then announce it.
    ///
    /// Record-before-deliver: the row exists before anything can act on
    /// it, so a crash between the two is a run the boot sweep finds rather
    /// than work that silently never happened. A refused enqueue means the
    /// issue already has a run in flight — the dedupe guard doing its job,
    /// not a failure the caller should see.
    async fn dispatch_if_triggered(&self, transition: Transition, issue: &IssueRow) {
        let Some(trigger) = triggers_run(transition) else {
            return;
        };
        self.enqueue(issue, trigger).await;
    }

    /// Record a run and start it, unless the board has spent its budget.
    ///
    /// The single enqueue path — a drag, a comment, a retry and a tool call
    /// all arrive here, so the gate cannot be forgotten on one of them.
    ///
    /// The order is deliberate: **the row is written before the budget is
    /// consulted**, so an exhausted board records work it owes rather than
    /// dropping it. The run lands `Held`, holds the issue's dedupe slot, and
    /// starts the moment there is headroom again. A refused write means the
    /// issue already has a run in flight — the dedupe guard doing its job,
    /// not a failure the caller should see.
    ///
    /// Whatever the board is already holding is released **before** that
    /// write, and deliberately so. This is the third release site — with a
    /// budget change and the boot sweep — and the one that makes a rolled-over
    /// ceiling need no timer. It cannot move after the write: on the board
    /// that actually needs releasing, the exhausted one, every card is holding
    /// its own dedupe slot, so every enqueue is refused and a release placed
    /// afterwards would never be reached. The price is that a caller whose own
    /// held run has just become affordable is told the issue already has a run
    /// in flight — which is true, and it is the run they asked for.
    async fn enqueue(&self, issue: &IssueRow, trigger: RunTrigger) -> Option<IssueRunRow> {
        let entry = ledger_entry(issue, trigger)?;
        let headroom = self.headroom(&issue.project_id).await;
        if let Err(e) = self.release_holds(&issue.project_id, headroom).await {
            tracing::error!(project = %issue.project_id, error = %e, "could not release held runs");
        }
        let run = match self.store.enqueue_run(&entry).await {
            Ok(run) => run,
            Err(baybo_store::StorageError::Conflict(reason)) => {
                tracing::debug!(
                    issue = issue.number,
                    %reason,
                    "issue already has a run in flight; not starting a second"
                );
                return None;
            }
            Err(e) => {
                tracing::error!(issue = issue.number, error = %e, "could not record issue run");
                return None;
            }
        };
        self.events.run_changed(&issue.project_id, issue.number);

        if let (true, Some((spent_micros, limit_micros))) =
            (headroom.is_exhausted(), headroom.figures())
        {
            if let Err(e) = self.store.hold_run(&run.id).await {
                // The hold failed, so the run is still `Queued` and will be
                // started. Overspending by one run beats stranding it.
                tracing::error!(issue = issue.number, error = %e, "could not hold a run over budget");
            } else {
                self.record(
                    issue,
                    IssueActor::System,
                    IssueEventBody::BudgetExhausted {
                        spent_micros,
                        limit_micros,
                    },
                )
                .await;
                return Some(run);
            }
        }
        (self.dispatch)(run.clone());
        Some(run)
    }

    /// This project's spend against its ceiling, for today.
    ///
    /// Fails **open**: a board that cannot be measured keeps working. The
    /// alternative is a storage hiccup silently pausing every project, which
    /// looks exactly like the product being broken.
    async fn headroom(&self, project: &ProjectId) -> Headroom {
        let limit = match self.store.get_project(project).await {
            Ok(Some(row)) => row.daily_budget,
            Ok(None) => return Headroom::Unlimited,
            Err(e) => {
                tracing::error!(%project, error = %e, "could not read the project's budget");
                return Headroom::Unlimited;
            }
        };
        // No ceiling means no query. The common case costs nothing.
        if limit.is_none() {
            return Headroom::Unlimited;
        }
        match self
            .store
            .spend_since(project, crate::day_start(chrono::Utc::now()))
            .await
        {
            Ok(spent) => crate::headroom(limit, spent),
            Err(e) => {
                tracing::error!(%project, error = %e, "could not read the project's spend");
                Headroom::Unlimited
            }
        }
    }

    /// Start whatever this board is holding, if it has room again.
    ///
    /// Released by activity on the board rather than by a clock: any
    /// enqueue, a budget change, and the boot sweep all pass through here.
    /// A daily ceiling that rolls over while nothing is happening needs no
    /// timer — the first thing that happens next releases the hold, and if
    /// nothing happens, nothing needed releasing.
    pub async fn release_held_runs(&self, project: &ProjectId) -> Result<usize> {
        let headroom = self.headroom(project).await;
        self.release_holds(project, headroom).await
    }

    /// Release every hold this headroom allows, and hand each out.
    ///
    /// Takes the headroom rather than reading it, so [`Self::enqueue`] —
    /// which has already measured the board to decide whether to hold — does
    /// not query the budget twice for one write.
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
            if !self.store.release_run(&run.id).await? {
                continue;
            }
            released += 1;
            self.events.run_changed(project, run.number);
            if let (Some((spent_micros, limit_micros)), Ok(issue)) =
                (figures, self.get_issue(project, run.number).await)
            {
                self.record(
                    &issue,
                    IssueActor::System,
                    IssueEventBody::BudgetRestored {
                        spent_micros,
                        limit_micros,
                    },
                )
                .await;
            }
            (self.dispatch)(run);
        }
        Ok(released)
    }

    /// Append to an issue's timeline, and tell whoever is watching.
    ///
    /// A failed append is logged and swallowed. The timeline is a record of
    /// work, not a gate on it: losing the note that a card moved is bad,
    /// and refusing the move because the note could not be written is
    /// worse.
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
    ///
    /// Public because writers outside this manager have entries to add —
    /// the approval gate, which sees prompts the board never asked for.
    /// Swallows a missing issue for the same reason [`Self::record`]
    /// swallows a failed append: losing the note is bad, and failing the
    /// thing the note was about is worse.
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

    /// Record whatever this edit is worth saying, if anything.
    async fn record_diff(&self, before: &IssueRow, after: &IssueRow, actor: IssueActor) {
        for body in crate::timeline::diff_events(before, after) {
            self.record(after, actor.clone(), body).await;
        }
    }

    /// Give the worktree back when an issue reaches Done or Cancelled.
    ///
    /// Only on the *transition*: re-saving a card that was already Done
    /// must not keep trying, and an issue reopened and finished again gets
    /// its second reclamation honestly.
    ///
    /// Nothing here fails the edit. Dragging a card to Done is a statement
    /// about the work, not a filesystem operation, and it stands whatever
    /// git says.
    async fn reclaim_if_finished(&self, before: &IssueRow, after: &IssueRow, actor: IssueActor) {
        let finished =
            |issue: &IssueRow| issue.status == IssueStatus::Done || issue.cancelled_at.is_some();
        if finished(before) || !finished(after) {
            return;
        }
        let Ok(Some(project)) = self.store.get_project(&after.project_id).await else {
            return;
        };
        let root = crate::worktree::worktree_root(&self.paths, &after.project_id, after.number);
        // The tree's own branch, not the one this issue's *current* title
        // implies: a retitle between the run and the reclamation would
        // otherwise delete-or-keep the wrong ref, or none at all.
        let branch = match crate::worktree::branch_of(&root).await {
            Some(branch) => branch,
            None => crate::worktree::branch_name(after.number, &after.title),
        };
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
    /// The comment always lands on the timeline; what else happens is
    /// [`crate::comment_delivery`]'s decision. Recording comes first in
    /// every branch, so a wake that fails is a comment that is still there
    /// to be read rather than one that was never said.
    pub async fn comment(
        &self,
        project: &ProjectId,
        number: i64,
        actor: IssueActor,
        text: &str,
    ) -> Result<IssueEventRow> {
        self.writable_project(project).await?;
        let issue = self.get_issue(project, number).await?;
        let text = text.trim();
        if text.is_empty() {
            return Err(ProjectError::invalid("text", "a comment cannot be empty"));
        }
        let entry = self
            .store
            .append_event(&NewIssueEvent {
                issue_id: issue.id.clone(),
                project_id: project.clone(),
                number,
                actor: actor.clone(),
                body: IssueEventBody::Comment {
                    text: text.to_owned(),
                },
            })
            .await?;
        self.events.timeline_changed(project, number);

        // An @mention on a card nobody is on is the commenter saying "you
        // take this". Applied after the comment is recorded, so the words
        // survive even if the assignment is refused — and applied through
        // `update_issue`, so it goes down the same path a drag does and
        // gets the same trigger, the same timeline entry, and the same
        // refusals for an agent that cannot run. In the commenter's name,
        // too: a handover a lead performed must not read as the operator's
        // on a card the operator is being asked to trust.
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

    /// Start the run a comment asked for, on an issue whose answer had to
    /// wait.
    ///
    /// The wake half of [`Self::comment`], reachable on its own because a
    /// comment left while a run was executing is deferred
    /// ([`CommentDelivery::AfterCurrentRun`]) — the executor calls this once
    /// that run settles and the issue's live-run slot is free again. It goes
    /// through [`Self::enqueue`] like every other start, so a deferred wake
    /// gets the same budget gate, the same dedupe guard and the same
    /// dispatch a drag does; a ledger row written straight to the store
    /// would be a run nothing ever starts, holding the slot until the next
    /// boot.
    ///
    /// `None` is an answer, not a failure: an issue cancelled, unassigned or
    /// dragged out of live work while the run was going — or one on a board
    /// archived meanwhile — is one nobody should be woken on.
    pub async fn wake_on_comment(&self, project: &ProjectId, number: i64) -> Option<IssueRunRow> {
        self.writable_project(project).await.ok()?;
        let issue = self.get_issue(project, number).await.ok()?;
        self.wake_if_listening(&issue).await
    }

    /// Enqueue for a comment if the issue is listening. One implementation,
    /// shared by [`Self::comment`] (which has the row in hand) and
    /// [`Self::wake_on_comment`] (which has to re-read it).
    async fn wake_if_listening(&self, issue: &IssueRow) -> Option<IssueRunRow> {
        if self.delivery_for(issue).await != CommentDelivery::Wake {
            return None;
        }
        self.enqueue(issue, RunTrigger::Comment).await
    }

    /// The teammate an @mention hands this card to, if it hands it to
    /// anybody.
    ///
    /// A handle that names nobody on this board resolves to `None` rather
    /// than to an error: the comment is still worth recording, and a typo
    /// in a mention should not refuse the sentence around it.
    async fn mention_assignment(
        &self,
        project: &ProjectId,
        issue: &IssueRow,
        text: &str,
    ) -> Option<AgentProfileId> {
        let handle = crate::assigns_to(issue.assignee.is_some(), text)?;
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
    ///
    /// Public so the composer can say it before the comment is sent: the
    /// difference between "somebody will read this" and "this is a note for
    /// later" is invisible in a text box, and a person who expects the first
    /// and gets the second waits for an answer nobody is sending.
    pub async fn comment_delivery(
        &self,
        project: &ProjectId,
        number: i64,
    ) -> Result<CommentDelivery> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.delivery_for(&issue).await)
    }

    async fn delivery_for(&self, issue: &IssueRow) -> CommentDelivery {
        let live = match self.store.list_runs(&issue.id).await {
            Ok(runs) => runs
                .into_iter()
                .find(|run| !run.status.is_settled())
                .map(|run| run.status),
            Err(e) => {
                // Fail towards doing nothing: a spurious wake starts an
                // agent on work nobody asked it to redo.
                tracing::error!(issue = issue.number, error = %e, "could not read runs for comment delivery");
                return CommentDelivery::RecordOnly;
            }
        };
        crate::comment_delivery(issue, live)
    }

    /// One issue's timeline, oldest first.
    pub async fn timeline(&self, project: &ProjectId, number: i64) -> Result<Vec<IssueEventRow>> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.store.list_events(&issue.id).await?)
    }

    /// Every run of one issue, newest first.
    pub async fn list_runs(&self, project: &ProjectId, number: i64) -> Result<Vec<IssueRunRow>> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.store.list_runs(&issue.id).await?)
    }

    /// Stop an issue's run.
    ///
    /// A run that never started is settled here and now — there is nothing
    /// to interrupt. A run already executing is *not* settled here: its
    /// session is returned so the caller can cancel the live turn, and the
    /// waiter watching that turn settles the row with `Cancelled`. Settling
    /// it from both ends would race, and the waiter is the one that knows
    /// whether the turn actually stopped.
    ///
    /// The **status**, not the session, is what says a turn is live: the boot
    /// sweep returns an interrupted run to the queue with the session it was
    /// claimed with, and there is no turn left in it to stop. Chasing that
    /// session instead would leave the row unsettled forever, blocking every
    /// later run on the issue.
    pub async fn cancel_run(
        &self,
        project: &ProjectId,
        number: i64,
    ) -> Result<Option<baybo_model::SessionId>> {
        let issue = self.get_issue(project, number).await?;
        let live = self
            .store
            .list_runs(&issue.id)
            .await?
            .into_iter()
            .find(|run| !run.status.is_settled());
        let Some(run) = live else {
            return Err(ProjectError::invalid(
                "run",
                "nothing is running on this issue",
            ));
        };
        match run.session_id.filter(|_| run.status == RunStatus::Running) {
            Some(session) => Ok(Some(session)),
            None => {
                self.store
                    .settle_run(&run.id, RunStatus::Cancelled, None)
                    .await?;
                self.events.run_changed(project, number);
                Ok(None)
            }
        }
    }

    /// Run an issue again. Refused while one is already in flight — the
    /// same dedupe guard a drag hits, surfaced as a conflict rather than a
    /// silent second agent.
    pub async fn retry_run(&self, project: &ProjectId, number: i64) -> Result<IssueRunRow> {
        self.writable_project(project).await?;
        let issue = self.get_issue(project, number).await?;
        if issue.assignee.is_none() {
            return Err(ProjectError::invalid(
                "assignee",
                "an issue with nobody on it cannot be run",
            ));
        }
        self.enqueue(&issue, RunTrigger::Retry)
            .await
            .ok_or_else(|| ProjectError::Conflict("this issue already has a run".to_owned()))
    }

    /// The unfinished runs of one board — which cards are working.
    pub async fn active_runs(&self, project: &ProjectId) -> Result<Vec<IssueRunRow>> {
        self.get_project(project).await?;
        Ok(self.store.active_runs(project).await?)
    }

    /// Return orphaned runs to the queue and hand each back for dispatch.
    /// Called once at boot, before live traffic: a `running` row whose
    /// actor died with the process is work that never finished.
    pub async fn resume_unsettled_runs(&self) -> Result<usize> {
        let resumed = self.store.requeue_unsettled().await?;
        let mut count = resumed.len();
        for run in resumed {
            (self.dispatch)(run);
        }
        // Held runs are not "orphaned" — they were never started on purpose
        // — so the sweep above leaves them alone. Boot is the one moment
        // guaranteed to happen after a budget rolls over, so re-evaluating
        // them here is what keeps a hold from outliving its day.
        for project in self.store.list_projects(false).await? {
            match self.release_held_runs(&project.id).await {
                Ok(released) => count += released,
                Err(e) => {
                    tracing::error!(project = %project.id, error = %e, "could not release held runs")
                }
            }
        }
        Ok(count)
    }

    pub async fn create_project(&self, new: NewProject) -> Result<ProjectRow> {
        let name = validate_name(&new.name)?;
        let id = ProjectId::generate();

        // Filesystem first, row second. A crash between the two leaves an
        // empty directory nobody references — inert. The other order leaves
        // a project pointing at somewhere that does not exist, which every
        // later run would have to handle.
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

        // Before the project row, not after. A lead that fails to
        // materialise this way leaves an agent row whose project does not
        // exist — invisible to both rosters and inert, like the orphaned
        // persona directory above. The other order leaves a *visible* board
        // with no coordinator, which is the state every other path here
        // assumes cannot happen.
        self.seed_lead(&id).await?;

        let now = chrono::Utc::now();
        let row = ProjectRow {
            id,
            name,
            description: new.description.trim().to_owned(),
            workdir,
            daily_budget: new.daily_budget,
            // Never read, so a board's first agent comment is unread even
            // if it lands before anybody opens it.
            read_at: None,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        self.store.create_project(&row).await?;
        self.events.project_changed(&row.id);
        Ok(row)
    }

    /// Staff a brand-new board with its coordinator.
    ///
    /// Every project has a lead, and it is created here rather than on first
    /// use so the invariant holds for every reader — the team strip, the
    /// triage loop, the chat panel — instead of each one having to handle a
    /// board that has nobody on it yet.
    async fn seed_lead(&self, project: &ProjectId) -> Result<()> {
        // A compile-time literal, so this can only fail if somebody edits
        // `LEAD_HANDLE` into something the grammar refuses — which the test
        // below catches long before a project is ever opened.
        let handle = AgentHandle::parse(LEAD_HANDLE)
            .map_err(|e| anyhow::anyhow!("LEAD_HANDLE is not a valid handle: {e}"))?;
        let id = AgentProfileId::generate();
        baybo_workspace::ensure_named_persona_layout(
            &self.paths,
            id.as_str(),
            baybo_workspace::prompt::PROJECT_LEAD_SOUL_TEMPLATE,
            LEAD_DISPLAY_NAME,
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
                llm: None,
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

    /// Put somebody new on a team.
    ///
    /// `hired_by` is `None` when the operator did it and `Some(agent)` when
    /// a teammate did — the lead staffing its own team. The distinction is
    /// kept on the row rather than in a log line because the profile panel
    /// shows it, and because a hiring loop is only auditable if each hire
    /// names who made it.
    ///
    /// Refusals are all things the caller can fix: a blank or unusable
    /// name, a role that is too long, a full team, or a name whose handle
    /// is already taken and stays taken.
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
        let base = AgentHandle::derive(&name).ok_or_else(|| {
            ProjectError::invalid(
                "name",
                format!(
                    "{name:?} has no letters or digits to make a handle from — \
                     an agent has to be addressable as @something"
                ),
            )
        })?;

        let id = AgentProfileId::generate();
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
        // The handle index is the arbiter, not the roster read above: two
        // hires racing for `@qa` both see it free, and only the loser needs
        // a different one. Suffixing on the conflict rather than checking
        // first is what makes that race correct instead of merely unlikely.
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
    ///
    /// A tombstone, never a delete: the agent's id is written into every
    /// issue it was assigned, every run it executed and every timeline entry
    /// it wrote, and the board has to keep being able to say who did what.
    ///
    /// Two refusals. The **lead** cannot leave — the board would have no
    /// coordinator, which nothing downstream is written to handle. An agent
    /// with a **run in flight** cannot leave either: the run keeps going
    /// (its row is what it reads, not the roster), so removing it now just
    /// hides who is doing the work that is happening. Cancel the run first.
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

    /// Create `work/<slug>/` and make it a git repository.
    ///
    /// An existing non-empty directory is an error rather than a silent
    /// reuse: `work/` is shared with the bash tool's scratch space, and
    /// adopting whatever is already sitting there would hand a project a
    /// working tree it did not create.
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
        let existing = self.get_project(id).await?;
        if existing.archived_at.is_some() {
            return Err(ProjectError::Archived(id.clone()));
        }
        let update = ProjectUpdate {
            name: validate_name(&update.name)?,
            description: update.description.trim().to_owned(),
            daily_budget: validate_budget(update.daily_budget)?,
        };
        self.store.update_project(id, &update).await?;
        self.events.project_changed(id);
        // A raised ceiling should start the work it was blocking, without
        // the operator having to touch each card.
        if let Err(e) = self.release_held_runs(id).await {
            tracing::error!(project = %id, error = %e, "could not release held runs after a budget change");
        }
        self.get_project(id).await
    }

    /// Archive or restore. Archiving is always allowed — including on an
    /// already-archived project, which is how a restore is idempotent.
    pub async fn set_project_archived(&self, id: &ProjectId, archived: bool) -> Result<ProjectRow> {
        if !self.store.set_project_archived(id, archived).await? {
            return Err(ProjectError::NoSuchProject(id.clone()));
        }
        self.events.project_changed(id);
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

    pub async fn create_issue(
        &self,
        project: &ProjectId,
        actor: IssueActor,
        new: NewIssueRequest,
    ) -> Result<Opened> {
        self.writable_project(project).await?;
        let title = validate_issue_title(&new.title)?;
        if let Some(assignee) = new.assignee.as_ref() {
            self.validate_assignee(project, assignee).await?;
        }
        self.validate_staffing(new.status, new.assignee.as_ref())?;
        let parent = match new.parent {
            Some(number) => Some(self.validate_parent(project, number, None).await?),
            None => None,
        };
        // Checked before the write and enforced by the index behind it. The
        // check alone would race; the index alone would surface as an
        // opaque conflict the caller could not distinguish from a real
        // failure. Both, so the common case answers with the standing card
        // and a race still cannot produce two.
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
                status: new.status,
                priority: new.priority,
                assignee: new.assignee,
                parent_issue_id: parent.as_ref().map(|p| p.id.clone()),
                stage: if parent.is_some() { new.stage } else { 0 },
                source_key: new.source_key,
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
        if let Some(assignee) = issue.assignee.clone() {
            self.record(
                &issue,
                actor,
                IssueEventBody::Assigned {
                    from: None,
                    to: Some(assignee),
                },
            )
            .await;
        }
        self.dispatch_if_triggered(Transition::created(&issue), &issue)
            .await;
        Ok(Opened::Created(issue))
    }

    pub async fn update_issue(
        &self,
        project: &ProjectId,
        number: i64,
        actor: IssueActor,
        update: IssueUpdate,
    ) -> Result<IssueRow> {
        self.writable_project(project).await?;
        if update.is_empty() {
            return Err(ProjectError::invalid("update", "sets no field"));
        }
        if let Some(Some(parent_id)) = update.parent.as_ref() {
            let parent = self.issue_by_id(project, parent_id).await?;
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
        let update = IssueUpdate {
            title: update
                .title
                .as_deref()
                .map(validate_issue_title)
                .transpose()?,
            description: update.description.map(|d| d.trim().to_owned()),
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
        self.dispatch_if_triggered(Transition::between(&before, &after), &after)
            .await;
        self.check_stage_barrier(&before, &after, actor).await;
        Ok(after)
    }

    /// Wake a parent when one of its stages just emptied.
    ///
    /// Only on the *transition* into a finished state, and only for a child:
    /// re-saving a Done sub-issue must not wake the parent again, and an
    /// ordinary card has no parent to wake.
    ///
    /// The wake goes through the same ledger as everything else — record,
    /// then dispatch — so a barrier that fires while the process dies is a
    /// run the boot sweep finds rather than a stage that silently never
    /// opened.
    ///
    /// A stage that empties while an earlier one is still open is not a
    /// barrier — see [`crate::stages::barrier_opens`]. The parent has
    /// nothing new to drive, and the wake would spend its single live-run
    /// slot, so the barrier that matters is later refused by the dedupe
    /// index. Nothing is recorded either: the entry's whole meaning is "your
    /// assignee was woken", and writing it when nobody was is the lie the
    /// entry exists to avoid.
    async fn check_stage_barrier(&self, before: &IssueRow, after: &IssueRow, actor: IssueActor) {
        let finished =
            |issue: &IssueRow| issue.status == IssueStatus::Done || issue.cancelled_at.is_some();
        if finished(before) || !finished(after) {
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
        if !crate::stages::barrier_opens(&children, after.stage) {
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
            actor,
            IssueEventBody::StageCompleted { stage: after.stage },
        )
        .await;
        // Nobody on the parent means nobody to wake. The event above still
        // lands, so the operator sees the stage opened and can staff it.
        if parent.assignee.is_some() {
            self.enqueue(&parent, RunTrigger::StageBarrier).await;
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
        self.dispatch_if_triggered(Transition::between(&before, &after), &after)
            .await;
        self.check_stage_barrier(&before, &after, actor).await;
        Ok(after)
    }

    /// One board's capacity right now: who is actually executing, what is
    /// parked on budget, and how much is left to spend.
    ///
    /// One typed read rather than three public accessors, because the rule
    /// that makes it correct — **a held run is not a busy agent** — has to
    /// live in one place. On an exhausted board the opposite reading
    /// inverts the truth exactly when it matters: the team is idle and the
    /// wallet is empty, but a caller counting held runs as work sees a
    /// saturated roster and stops promoting, so the hold is never noticed.
    ///
    /// `headroom` fails open, so a board that cannot be measured reports
    /// `Unlimited` — which is what the enqueue gate acts on too, so a
    /// reader is told the same story the gate believes.
    pub async fn board_load(&self, project: &ProjectId) -> Result<BoardLoad> {
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
    ///
    /// `pending_approval_sessions` is the channel's live approval queue,
    /// passed in rather than reached for: this crate must not know about
    /// channels, and the queue is the authority on which prompts are still
    /// open — it cannot show one that already timed out.
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

    /// Note that the operator has looked at this board.
    ///
    /// Called when the board page loads, and only there: the detail route
    /// is one card, and marking the whole board read from it would clear a
    /// question asked on a card the operator never saw.
    pub async fn mark_read(&self, project: &ProjectId) -> Result<()> {
        self.get_project(project).await?;
        self.store
            .mark_project_read(project, chrono::Utc::now())
            .await?;
        Ok(())
    }

    /// The whole board's activity, newest first.
    ///
    /// Capped rather than paged-without-limit: a feed is read from the top
    /// and abandoned, and an unbounded read of a year-old board is a
    /// request nobody wanted to make.
    pub async fn feed(
        &self,
        project: &ProjectId,
        before: Option<chrono::DateTime<chrono::Utc>>,
        limit: usize,
    ) -> Result<Vec<IssueEventRow>> {
        self.get_project(project).await?;
        Ok(self
            .store
            .project_feed(project, before, limit.clamp(1, MAX_FEED_PAGE))
            .await?)
    }

    /// One issue's sub-issues, by stage then position.
    pub async fn children(&self, project: &ProjectId, number: i64) -> Result<Vec<IssueRow>> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.store.list_children(&issue.id).await?)
    }

    /// Resolve and check a proposed parent.
    ///
    /// Three refusals, all of which would otherwise produce a board nobody
    /// can read: a parent on another project (or missing), an issue made a
    /// step of itself, and a second level of nesting — either because the
    /// proposed parent is already a child, or because the issue being
    /// re-parented has children of its own. One level is a decision, and
    /// this is where it is kept.
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

    /// This board's issue with that ULID.
    ///
    /// Scoped on purpose: `IssueUpdate::parent` carries an id, so without
    /// this a caller could make one board's card a step of another's. The
    /// lookup is by list rather than by a store method because the scope
    /// *is* the check — a `get_by_id` would be the thing that lets it
    /// through.
    pub async fn issue_by_id(&self, project: &ProjectId, id: &IssueId) -> Result<IssueRow> {
        self.store
            .list_issues(project)
            .await?
            .into_iter()
            .find(|issue| &issue.id == id)
            .ok_or_else(|| ProjectError::invalid("parent", format!("no issue {id} on this board")))
    }

    /// `ordered_numbers` has to be the destination column's **whole** new
    /// membership: every card that will be in it once the move lands, and
    /// nothing else.
    ///
    /// Checked rather than assumed, because the store renumbers exactly the
    /// numbers it is handed and leaves every other row's `position` alone.
    /// A caller that sends the list it is *showing* — a board with cancelled
    /// cards hidden, or any later filter — omits precisely the rows nobody
    /// is looking at, and those keep a stale rank that now collides with a
    /// renumbered one. Nothing downstream reads `position` for anything but
    /// sorting, so the corruption is silent and permanent.
    ///
    /// A refusal is not a cost here: every legitimate caller already knows
    /// the whole column, because it just read the board to compute the
    /// order.
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

    /// An assignee has to be an agent on *this* board that can actually run.
    ///
    /// Three refusals, each for work that could never happen:
    ///
    /// - **Not on the team.** A global chat persona has no handle here, is
    ///   absent from the team strip, and cannot be mentioned — assigning one
    ///   produces a card addressed to somebody the board cannot name.
    /// - **Removed.** Its row survives so the timeline can still say what it
    ///   did, which is exactly why the tombstone has to be checked here: a
    ///   `get` that resolves is not a member who can be given new work.
    /// - **External framework.** A top-level session cannot be bound to a
    ///   non-baybo framework today — the external backend exists only inside
    ///   the subagent spawner — so the card could never start.
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
        if profile.framework != AgentFramework::Baybo {
            return Err(ProjectError::invalid(
                "assignee",
                format!(
                    "{assignee} runs on {}, which cannot yet host an issue's session — \
                     assign a baybo agent",
                    profile.framework.as_str()
                ),
            ));
        }
        Ok(())
    }

    /// In Progress means somebody is on it. A card in that column with no
    /// assignee is work the board claims is happening and nobody is doing;
    /// every other column is free to be unassigned.
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

    /// Resolve a project and refuse if it is archived. Every write path
    /// starts here, so "archived is read-only" is one rule in one place
    /// rather than a check each endpoint has to remember.
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

/// A ceiling has to be something a board can actually spend against.
///
/// Negative is refused rather than clamped: it is a caller mistake, and
/// clamping it to zero would silently pause every board on that project.
fn validate_budget(budget: Option<baybo_model::MicroUsd>) -> Result<Option<baybo_model::MicroUsd>> {
    if budget.is_some_and(|b| b.into_micros() < 0) {
        return Err(ProjectError::invalid(
            "daily_budget",
            "must not be negative — use 0 to pause the board, or leave it unset for no limit",
        ));
    }
    Ok(budget)
}

fn validate_agent_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ProjectError::invalid("name", "must not be empty"));
    }
    if trimmed.chars().count() > baybo_model::MAX_AGENT_PROFILE_NAME_CHARS {
        return Err(ProjectError::invalid(
            "name",
            format!(
                "longer than {} characters",
                baybo_model::MAX_AGENT_PROFILE_NAME_CHARS
            ),
        ));
    }
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

/// `base` with `-n` appended, truncated so the result still fits the handle
/// grammar's length bound.
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
///
/// A project's checkout is bound read-write into every shell command its
/// team runs, so it must be neither inside the workspace tree nor a parent
/// of it.
pub fn validate_workdir(workdir: &str, paths: &WorkspacePaths) -> Result<()> {
    let path = Path::new(workdir);
    if !path.is_absolute() {
        return Err(ProjectError::invalid("workdir", "must be an absolute path"));
    }
    // Canonicalise before comparing, because the sandbox resolves symlinks
    // when it mounts: a lexical-only check passes a link aimed at `state/`
    // and then binds exactly what it refused. A path that does not exist
    // yet can only be checked lexically — and cannot be bound either.
    let resolve = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let target = resolve(path);
    let root = resolve(&baybo_workspace::absolutise(paths.root()));
    let work = resolve(&baybo_workspace::absolutise(&paths.work_dir()));
    // Both directions. A descendant is the obvious one; an **ancestor** is
    // the one that bites, because the default workspace lives in `~/.baybo`
    // and `workdir = $HOME` would bind the whole home directory — baybo's
    // own state, keys and all — read-write into every shell the team runs.
    // `work/` is the single exemption: a shell may already write there.
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
