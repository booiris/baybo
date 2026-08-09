//! Project and issue lifecycle: validation, workdir materialisation, and
//! the board's write surface.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use baybo_model::{
    AgentFramework, AgentHandle, AgentProfileId, IssueId, MAX_PROJECT_NAME_CHARS, ProjectId,
    SessionId, TeamMembership,
};
use baybo_store::AgentProfileStore;
use baybo_store::project::{
    IssueActor, IssueEventBody, IssueEventRow, IssuePriority, IssueRow, IssueRunRow, IssueStatus,
    IssueUpdate, NewIssue, NewIssueEvent, ProjectRow, ProjectStore, ProjectUpdate, RunStatus,
    RunTrigger,
};
use baybo_workspace::WorkspacePaths;
use chrono::{DateTime, Utc};

use crate::CommentDelivery;
use crate::budget::Headroom;
use crate::error::{ProjectError, Result};
use crate::events::ProjectEvents;
use crate::runs::{RunOutcome, Transition, ledger_entry, triggers_run};

/// Upper bound on an issue title (chars, after trim). Long enough for a
/// sentence, short enough that a card face can show it.
pub(crate) const MAX_ISSUE_TITLE_CHARS: usize = 200;

/// The handle every project's coordinator answers to. Fixed rather than
/// derived: `@lead` means the same thing on every board, and it is the one
/// handle a person can type without looking the team up first.
pub const LEAD_HANDLE: &str = "lead";

const LEAD_DISPLAY_NAME: &str = "Lead";

const LEAD_DESCRIPTION: &str =
    "Coordinates this project's board: triages Backlog, assigns work, and staffs the team.";

/// How many live agents one project may have, lead included.
pub const MAX_TEAM_AGENTS: usize = 16;

/// Upper bound on a teammate's role line (chars, after trim). It seeds a
/// SOUL and shows on a roster card, so it is a sentence, not a brief.
pub(crate) const MAX_ROLE_CHARS: usize = 280;

/// Upper bound on one activity-feed page.
pub const MAX_FEED_PAGE: usize = 100;

const RUN_CALLED_OFF_UNSTARTED: &str = "the card was finished or cancelled before this run started";

const RUN_CALLED_OFF_INTERRUPTED: &str =
    "this run was interrupted, and the card was finished or cancelled before it could resume";

const RUN_CANCELLED_BEFORE_STARTING: &str = "cancelled before it started";

const HELD_RUN_REFUSAL: &str =
    "this run is held — the project is over its daily budget, and starts as soon as there is room";

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

    async fn dispatch_if_triggered(&self, transition: Transition, issue: &IssueRow) {
        let Some(trigger) = triggers_run(transition) else {
            return;
        };
        self.enqueue(issue, trigger).await;
    }

    async fn enqueue(&self, issue: &IssueRow, trigger: RunTrigger) -> Option<IssueRunRow> {
        if !crate::runs::accepts_runs(issue) {
            tracing::debug!(
                issue = issue.number,
                ?trigger,
                "the card is finished or cancelled; not starting a run on it"
            );
            return None;
        }
        if self.assignee_can_run(issue).await != Some(true) {
            tracing::debug!(
                issue = issue.number,
                ?trigger,
                "the assignee cannot host an issue's session; not starting a run"
            );
            return None;
        }
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
            .spend_since(project, crate::budget::day_start(chrono::Utc::now()))
            .await
        {
            Ok(spent) => crate::budget::headroom(limit, spent),
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
            // A hold outlives the write that recorded it, so the card may
            // have been cancelled or finished in between — and its worktree
            // reclaimed with it.
            let Some(issue) = self.live_card(&run).await else {
                continue;
            };
            if !self.store.release_run(&run.id).await? {
                continue;
            }
            released += 1;
            self.events.run_changed(project, run.number);
            if let Some((spent_micros, limit_micros)) = figures {
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

    async fn live_card(&self, run: &IssueRunRow) -> Option<IssueRow> {
        match self.store.get_issue(&run.project_id, run.number).await {
            Ok(Some(issue)) if crate::runs::accepts_runs(&issue) => Some(issue),
            Ok(_) => {
                self.call_off(run).await;
                None
            }
            Err(e) => {
                tracing::error!(issue = run.number, error = %e, "could not read the card a recorded run belongs to; leaving the run where it is");
                None
            }
        }
    }

    async fn call_off(&self, run: &IssueRunRow) {
        let reason = call_off_reason(run);
        if let Err(e) = crate::settle::settle_run(
            &self.store,
            &self.events,
            run,
            IssueActor::System,
            RunStatus::Cancelled,
            Some(reason),
        )
        .await
        {
            tracing::error!(issue = run.number, error = %e, "could not call off a run on a finished card");
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
        if let Err(e) = crate::settle::settle_run(
            &self.store,
            &self.events,
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
    /// The board never merges, so this ref is the artefact it hands the
    /// operator. It is read from the checkout rather than derived from the
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
    /// The run's own comments are skipped as well — an agent reporting
    /// progress is not somebody asking it for more.
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

    async fn was_told_something_during(
        &self,
        run: &IssueRunRow,
        briefed_at: DateTime<Utc>,
    ) -> bool {
        // Keyed on the profile, not on the run that wrote it, because a
        // timeline entry records only its actor. One agent holding two live
        // cards can therefore comment from one onto the other and have it
        // skipped here. Narrowing it needs the authoring run on the event
        // body, which is a stored-shape change; until then this errs
        // towards a missed nudge rather than an agent that answers its own
        // progress note and wakes itself again on the answer.
        let own = IssueActor::Agent(run.agent_id.clone());
        match self.store.events_since(&run.issue_id, briefed_at).await {
            Ok(events) => events
                .iter()
                .any(|e| matches!(e.body, IssueEventBody::Comment { .. }) && e.actor != own),
            Err(e) => {
                tracing::warn!(run = %run.id, error = %e, "could not check for comments left during the run");
                false
            }
        }
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
    pub async fn wake_on_comment(&self, project: &ProjectId, number: i64) -> Option<IssueRunRow> {
        self.writable_project(project).await.ok()?;
        let issue = self.get_issue(project, number).await.ok()?;
        self.wake_if_listening(&issue).await
    }

    async fn wake_if_listening(&self, issue: &IssueRow) -> Option<IssueRunRow> {
        if self.delivery_for(issue).await != CommentDelivery::Wake {
            return None;
        }
        self.enqueue(issue, RunTrigger::Comment).await
    }

    async fn mention_assignment(
        &self,
        project: &ProjectId,
        issue: &IssueRow,
        text: &str,
    ) -> Option<AgentProfileId> {
        let handle = crate::mentions::assigns_to(issue.assignee.is_some(), text)?;
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
            .find(|run| !run.status.is_settled()))
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

    /// Every run of one issue, newest first.
    pub async fn list_runs(&self, project: &ProjectId, number: i64) -> Result<Vec<IssueRunRow>> {
        let issue = self.get_issue(project, number).await?;
        Ok(self.store.list_runs(&issue.id).await?)
    }

    /// Stop an issue's run.
    pub async fn cancel_run(
        &self,
        project: &ProjectId,
        number: i64,
    ) -> Result<Option<baybo_model::SessionId>> {
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
        let live_session = run
            .session_id
            .clone()
            .filter(|_| run.status == RunStatus::Running);
        match live_session {
            Some(session) => Ok(Some(session)),
            None => {
                crate::settle::settle_run(
                    &self.store,
                    &self.events,
                    &run,
                    IssueActor::User,
                    RunStatus::Cancelled,
                    Some(RUN_CANCELLED_BEFORE_STARTING),
                )
                .await?;
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
        let held = self
            .live_run(&issue)
            .await?
            .filter(|run| run.status == RunStatus::Held);

        if let Some(run) = self.enqueue(&issue, RunTrigger::Retry).await {
            return Ok(run);
        }
        match held {
            Some(held) => match self.live_run(&issue).await? {
                Some(run) if run.id == held.id && run.status != RunStatus::Held => Ok(run),
                _ => Err(ProjectError::Conflict(HELD_RUN_REFUSAL.to_owned())),
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
        self.store.requeue_unsettled().await?;
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
            if run.status != RunStatus::Queued || self.live_card(&run).await.is_none() {
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

    /// The profile rows for `ids` **on this board**, reaching agents that
    /// have left its team.
    pub(crate) async fn agent_profiles(
        &self,
        project: &ProjectId,
        ids: impl IntoIterator<Item = AgentProfileId>,
    ) -> Vec<baybo_store::AgentProfileRow> {
        let mut rows = Vec::new();
        for id in ids {
            match self.agents.get(&id).await {
                Ok(Some(row))
                    if row
                        .team
                        .as_ref()
                        .is_some_and(|team| &team.project_id == project) =>
                {
                    rows.push(row)
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(agent = %id, error = %e, "could not resolve an agent a timeline names")
                }
            }
        }
        rows
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
        let base = AgentHandle::derive(&name).ok_or_else(|| {
            ProjectError::invalid(
                "name",
                format!(
                    "{name:?} has no letters or digits to make a handle from — \
                     an agent has to be addressable as @something"
                ),
            )
        })?;

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
        self.writable_project(id).await?;
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
            actor,
            IssueEventBody::StageCompleted { stage: after.stage },
        )
        .await;
        // Nobody on the parent means nobody to wake. The event above still
        // lands, so the operator sees the stage opened and can staff it.
        if parent.assignee.is_some() && crate::stages::barrier_opens(&children, after.stage) {
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
    pub async fn mark_read(&self, project: &ProjectId) -> Result<()> {
        self.get_project(project).await?;
        self.store
            .mark_project_read(project, chrono::Utc::now())
            .await?;
        Ok(())
    }

    /// The whole board's activity, newest first.
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
    pub async fn issue_by_id(&self, project: &ProjectId, id: &IssueId) -> Result<IssueRow> {
        self.store
            .list_issues(project)
            .await?
            .into_iter()
            .find(|issue| &issue.id == id)
            .ok_or_else(|| ProjectError::invalid("parent", format!("no issue {id} on this board")))
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

    async fn assignee_can_run(&self, issue: &IssueRow) -> Option<bool> {
        let assignee = issue.assignee.as_ref()?;
        match self.agents.get(assignee).await {
            Ok(Some(profile)) => Some(crate::runs::can_host_a_session(profile.framework)),
            Ok(None) => Some(false),
            Err(e) => {
                tracing::error!(%assignee, error = %e, "could not read the assignee's framework");
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
