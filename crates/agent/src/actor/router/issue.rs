//! Executing a board issue's run.
//!
//! The ledger row already exists — `baybo-project` wrote it before anything
//! was told about it. This module is the consumer: it claims the row, mints
//! or reuses the issue's session, runs the brief as one turn, and settles
//! the row with what happened.
//!
//! The shape is cron's one-shot fire (`cron.rs`), with two differences that
//! matter. An issue keeps **one session per agent that works it**, so a
//! follow-up sees what the last one did; that is safe only because at most
//! one run per issue is ever in flight (a partial unique index enforces
//! it), which is what lets the waiter treat the terminal turn it sees as
//! unambiguously its own. And nothing is dispatched to a channel: an
//! issue's audience is its card.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use baybo_model::{
    AgentBinding, AgentProfileId, ChannelType, Session, SessionId, TriggerSource, User,
};
use baybo_project::{ProjectEvents, ProjectManager, worktree};
use baybo_store::project::{
    IssueActor, IssueEventBody, IssueRunRow, NewIssueEvent, ProjectStore, RunStatus,
};
use baybo_turn::{TurnInputKind, TurnLifecycle, TurnLifecycleEvent, TurnStatusKind};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::actor::AgentMessage;

use super::Router;

/// What a run needs to execute, as the dispatcher hands it over.
#[derive(Debug, Clone)]
pub struct IssueRunEvent {
    pub run: IssueRunRow,
    /// The brief the assignee is asked to work on — the issue's title and
    /// description, assembled by the caller so this module never has to
    /// know the issue's shape.
    pub brief: String,
    /// The worktree this run works in, already cut by the dispatcher.
    ///
    /// Prepared there rather than here on purpose: `git worktree add`
    /// shells out, and this handler is awaited directly on the router's
    /// `select!` loop — the same loop that serves every user message and
    /// every agent response. A slow checkout must not be head-of-line
    /// blocking for the whole process.
    pub checkout: PathBuf,
    /// Who the run belongs to, for session ownership.
    pub user_id: String,
    pub channel: ChannelType,
}

/// Everything executing a run needs from the board, as one value.
///
/// One `Option` rather than three, because no assembly produces any of
/// these without the others: the store settles the ledger row, the events
/// hook tells whoever is watching the card, and the manager starts whatever
/// the settle turns out to owe. Three independent `Option`s made the bad
/// combinations representable and left the invariant to a comment — and one
/// of them (a store with no manager) refused a run that had already been
/// dispatched, returning without settling its row, which is exactly the
/// shape the per-issue live index turns into a permanently stuck card.
#[derive(Clone)]
pub struct BoardWiring {
    pub store: Arc<dyn ProjectStore>,
    pub events: Arc<dyn ProjectEvents>,
    pub manager: Arc<ProjectManager>,
}

impl Router {
    pub(super) async fn handle_issue_run(&mut self, event: IssueRunEvent) {
        let run_id = event.run.id.clone();
        // The only refusal left, and it settles nothing on purpose: with no
        // board there is no store this row lives in. An assembly without one
        // (the TUI's runtime) also has no `issue_run_rx`, so nothing can
        // arrive here in the first place.
        let Some(board) = self.board.clone() else {
            warn!(%run_id, "issue run arrived with no board wiring; cannot execute");
            return;
        };
        let store = &board.store;

        let session = match self.issue_session(store, &event).await {
            Ok(session) => session,
            Err(e) => {
                warn!(%run_id, error = %e, "could not open the issue's session");
                settle(
                    store,
                    &board.events,
                    &event.run,
                    RunStatus::Failed,
                    Some(&e.to_string()),
                )
                .await;
                return;
            }
        };

        // Claim before dispatching. A run already claimed by another
        // dispatch of the same row — a boot sweep racing a live enqueue —
        // stops here, which is how a double dispatch collapses into one
        // execution rather than two agents on one card.
        match store.claim_run(&run_id, &session.id).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(%run_id, "run was already claimed; not executing it twice");
                return;
            }
            Err(e) => {
                warn!(%run_id, error = %e, "could not claim run");
                return;
            }
        }

        let checkout = event.checkout.clone();

        record(
            store,
            &board.events,
            &event.run,
            IssueEventBody::RunStarted {
                run_id: event.run.id.clone(),
                attempt: event.run.attempt,
                trigger: event.run.trigger,
            },
        )
        .await;

        // Subscribed before the trigger is sent, and this is load-bearing:
        // the terminal event is published from inside the run's own turn, so
        // a subscription opened afterwards can miss a fast failure entirely.
        let waiter = IssueRunWaiter {
            run: event.run.clone(),
            checkout: checkout.clone(),
            session_id: session.id.clone(),
            lifecycle: Arc::clone(&self.turn_lifecycle),
            terminal_rx: self.turn_lifecycle.subscribe_lifecycle_events(),
            board: board.clone(),
        };

        let pins = super::resolve_spawn_pins(&session, &self.agent_profiles).await;
        let response_tx = self.supervisor.response_tx().clone();
        let (mailbox, actor_token) = self.spawn_oneshot_actor(
            session,
            pins.llm,
            pins.model,
            pins.effort,
            response_tx,
            &self.actor_parent_token,
        );

        let trigger = AgentMessage::IssueRun {
            run_id: run_id.clone(),
            number: event.run.number,
            brief: event.brief.clone(),
            checkout: checkout.to_string_lossy().into_owned(),
        };
        match mailbox.send(trigger).await {
            Ok(()) => {
                // Lowest priority, so it is served after the run: the actor
                // exits as soon as its one turn is done.
                if let Err(e) = mailbox.send(AgentMessage::ActorStop).await {
                    warn!(%run_id, error = %e, "post-run shutdown was not queued");
                }
            }
            Err(e) => {
                warn!(%run_id, error = %e, "run could not reach its actor");
                // Cancel rather than return: the waiter is spawned either
                // way, and cancelling makes it resolve immediately. A run
                // nobody resolves is a card that shimmers forever.
                actor_token.cancel();
            }
        }

        tokio::spawn(waiter.run(actor_token));
    }

    /// The issue's session for the agent this run belongs to — reused
    /// across that agent's runs, minted on its first.
    ///
    /// A session's [`AgentBinding`] is write-once: it selects the persona
    /// and SOUL the turn runs as, the skills it may reach, and the name its
    /// commits are authored with, and none of those may change mid-thread.
    /// So the session cannot follow a reassignment — the *run* has to. A run
    /// handed to a session bound to somebody else would load the previous
    /// assignee's persona and sign that agent's name onto work the board
    /// says belongs to the new one, which is worse than an unattributed
    /// commit because it names the wrong somebody.
    ///
    /// Continuity is per agent, not per issue: an agent that already worked
    /// this card continues in the session it worked it in, however many
    /// hands the card has passed through since. Newest run first
    /// (`list_runs` orders by attempt), so the reasonable answer for a card
    /// reassigned back and forth is the most recent of that agent's own
    /// sessions.
    async fn issue_session(
        &self,
        store: &Arc<dyn ProjectStore>,
        event: &IssueRunEvent,
    ) -> anyhow::Result<Session> {
        let issue = store
            .get_issue(&event.run.project_id, event.run.number)
            .await?
            .ok_or_else(|| anyhow::anyhow!("issue #{} is gone", event.run.number))?;

        let mut candidates: Vec<SessionId> = Vec::new();
        for previous in store.list_runs(&issue.id).await? {
            if let Some(id) = previous.session_id
                && !candidates.contains(&id)
            {
                candidates.push(id);
            }
        }
        for candidate in candidates {
            if let Some(session) = self.session_manager.get(&candidate).await?
                && session.state.agent_id_or_builtin() == event.run.agent_id
            {
                return Ok(session);
            }
        }

        let user = User {
            id: event.user_id.clone(),
            name: None,
            channel: event.channel.clone(),
        };
        Ok(self
            .session_manager
            .create_bound_session_with_trigger(
                user,
                event.channel.clone(),
                TriggerSource::Issue {
                    project_id: issue.project_id.clone(),
                    issue_id: issue.id.clone(),
                    number: issue.number,
                },
                Some(binding_for(&event.run.agent_id)),
            )
            .await?)
    }
}

fn binding_for(agent: &AgentProfileId) -> AgentBinding {
    AgentBinding {
        agent_id: agent.clone(),
        framework: baybo_model::AgentFramework::Baybo,
    }
}

/// Start a follow-up run if anybody said anything while this one was
/// executing.
///
/// A comment that arrives mid-run is past the point where its brief was
/// assembled, and a one-shot actor has no mailbox anybody can reach — so
/// the comment waits here rather than being lost. This terminates: the
/// follow-up only looks at comments newer than its own predecessor's
/// start, so a quiet issue stops after one.
///
/// Started through the manager rather than written to the store here. That
/// is the one path that consults the board's budget, refuses a second live
/// run and hands the row to the dispatcher; a row written directly would be
/// a run nothing ever starts, holding the issue's only live-run slot until
/// the next boot.
///
/// A run the operator **cancelled** starts nothing, whatever was said. The
/// card is still InProgress and still assigned, so the board would happily
/// answer `Wake` — and a fresh run seconds after somebody pressed Cancel
/// reads as the Stop button not working. Every other terminal status does
/// follow up: a run that failed with a comment waiting is exactly the case
/// the follow-up exists for.
async fn follow_up_on_comments(
    projects: &Arc<ProjectManager>,
    store: &Arc<dyn ProjectStore>,
    run: &IssueRunRow,
    settled: RunStatus,
) {
    if settled == RunStatus::Cancelled {
        debug!(run = %run.id, "the run was cancelled; not starting a follow-up on it");
        return;
    }
    let said = match store.events_since(&run.issue_id, run.created_at).await {
        Ok(events) => events
            .into_iter()
            .any(|e| matches!(e.body, IssueEventBody::Comment { .. })),
        Err(e) => {
            warn!(run = %run.id, error = %e, "could not check for comments left during the run");
            return;
        }
    };
    if !said {
        return;
    }
    // Whether that still wakes anybody is the board's call, not this one's:
    // the issue may have been cancelled, unassigned or dragged out of live
    // work while the run was going, and the board goes out of budget or is
    // archived on its own schedule.
    match projects.wake_on_comment(&run.project_id, run.number).await {
        // The board may record the follow-up without starting it: an
        // exhausted daily budget parks it `Held` until headroom returns.
        // Saying "queued" for that is how an operator concludes the board
        // is stuck when it is merely waiting.
        Some(next) if next.status == RunStatus::Held => {
            info!(run = %run.id, next = %next.id, "a comment arrived mid-run; the follow-up is held until the board has budget")
        }
        Some(next) => {
            info!(run = %run.id, next = %next.id, "a comment arrived mid-run; queued a follow-up")
        }
        None => {
            debug!(run = %run.id, "a comment arrived mid-run, but the issue is no longer listening")
        }
    }
}

/// Record the branch on the issue once the work has actually produced
/// something.
///
/// Worktree and branch are separate ideas: every run gets a worktree, but
/// the branch is the *deliverable*, and an issue whose answer was a report
/// rather than code should show no branch anywhere. Keying the row's
/// `branch` on "has a commit" rather than storing it at creation is what
/// makes that fall out — there is no second flag that could disagree.
///
/// A count git could not take reads the same way as no commits at all: a
/// branch chip is a claim that there is something to look at, and an
/// unverified one is worse than none.
async fn surface_branch(
    store: &Arc<dyn ProjectStore>,
    events: &Arc<dyn ProjectEvents>,
    checkout: &Path,
    run: &IssueRunRow,
) {
    let (Ok(Some(issue)), Ok(Some(project))) = (
        store.get_issue(&run.project_id, run.number).await,
        store.get_project(&run.project_id).await,
    ) else {
        return;
    };
    if issue.branch.is_some() {
        return;
    }
    // The worktree's own branch. Recomputing it from the title would
    // record the wrong name for an issue renamed since its first run.
    let Some(branch) = worktree::branch_of(checkout).await else {
        return;
    };
    if worktree::commits_ahead(Path::new(&project.workdir), &branch)
        .await
        .is_none_or(|ahead| ahead == 0)
    {
        return;
    }
    match store.set_issue_branch(&issue.id, &branch).await {
        Ok(true) => events.board_changed(&issue.project_id, Some(issue.number)),
        Ok(false) => {}
        Err(e) => warn!(run = %run.id, error = %e, "could not record the issue's branch"),
    }
}

async fn settle(
    store: &Arc<dyn ProjectStore>,
    events: &Arc<dyn ProjectEvents>,
    run: &IssueRunRow,
    status: RunStatus,
    error: Option<&str>,
) {
    match store.settle_run(&run.id, status, error).await {
        // Announce only a settle that actually landed: a replay of an
        // already-settled run changed nothing and has nothing to say —
        // and must not put a second entry on the timeline either.
        Ok(true) => {
            events.run_changed(&run.project_id, run.number);
            record(
                store,
                events,
                run,
                IssueEventBody::RunSettled {
                    run_id: run.id.clone(),
                    attempt: run.attempt,
                    status,
                    error: error.map(str::to_owned),
                },
            )
            .await;
        }
        Ok(false) => {}
        Err(e) => {
            let run_id = &run.id;
            warn!(%run_id, error = %e, "could not settle run; the boot sweep will retry it");
        }
    }
}

/// Put a run's lifecycle on the issue's timeline, attributed to the agent
/// doing the work rather than to the operator who dragged the card.
///
/// Best-effort, like every other timeline write: a run that executed and
/// whose note did not land is far better than the reverse.
async fn record(
    store: &Arc<dyn ProjectStore>,
    events: &Arc<dyn ProjectEvents>,
    run: &IssueRunRow,
    body: IssueEventBody,
) {
    let entry = NewIssueEvent {
        issue_id: run.issue_id.clone(),
        project_id: run.project_id.clone(),
        number: run.number,
        actor: IssueActor::Agent(run.agent_id.clone()),
        body,
    };
    match store.append_event(&entry).await {
        Ok(_) => events.timeline_changed(&run.project_id, run.number),
        Err(e) => {
            let run_id = &run.id;
            warn!(%run_id, error = %e, "could not record a run's timeline entry");
        }
    }
}

/// Watches one run's turn and settles its ledger row.
struct IssueRunWaiter {
    run: IssueRunRow,
    /// The worktree this run worked in — asked for its branch once the run
    /// is over, because that is the authoritative name.
    checkout: PathBuf,
    session_id: SessionId,
    lifecycle: Arc<TurnLifecycle>,
    terminal_rx: broadcast::Receiver<TurnLifecycleEvent>,
    /// The board. Carries the manager, not just a store handle, because the
    /// one thing settling a run can owe — a follow-up for a comment that
    /// landed mid-run — has to go through the same enqueue, and therefore
    /// the same budget gate and the same dispatcher, as every other run.
    board: BoardWiring,
}

impl IssueRunWaiter {
    async fn run(mut self, actor_token: CancellationToken) {
        let (status, error) = self.await_run(actor_token).await;
        let run_id = &self.run.id;
        info!(%run_id, ?status, "issue run settled");
        let store = &self.board.store;
        settle(
            store,
            &self.board.events,
            &self.run,
            status,
            error.as_deref(),
        )
        .await;

        // After the row is settled, not before: the per-issue live index
        // would refuse a follow-up while this run still held the slot.
        // Only here, and not in `settle` itself — the two early-failure
        // settles above never started anything, so there is no branch to
        // surface and nothing that could have been said mid-run.
        //
        // Branch first, follow-up second. Both only read git, so the worst
        // an overlap costs is a missing chip — but a follow-up run works in
        // this same worktree, and reading it while somebody else is in it
        // is a race that need not exist.
        surface_branch(store, &self.board.events, &self.checkout, &self.run).await;
        follow_up_on_comments(&self.board.manager, store, &self.run, status).await;
    }

    async fn await_run(&mut self, actor_token: CancellationToken) -> (RunStatus, Option<String>) {
        loop {
            tokio::select! {
                event = self.terminal_rx.recv() => match event {
                    Ok(ev) if self.is_our_run(&ev) => {
                        let Some(kind) = ev.phase.terminal_status() else {
                            continue;
                        };
                        return outcome_for(kind);
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            run_id = %self.run.id,
                            skipped = n,
                            "issue waiter lagged on the lifecycle bus; reconciling via store"
                        );
                        if let Some(outcome) = self.reconcile().await {
                            return outcome;
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return (
                            RunStatus::Failed,
                            Some("lifecycle bus closed before the run finished".to_owned()),
                        );
                    }
                },
                _ = actor_token.cancelled() => {
                    // The actor is gone: it either finished and we have not
                    // drained its event, or it died before opening a turn.
                    // The store says which.
                    if let Some(outcome) = self.reconcile().await {
                        return outcome;
                    }
                    return (
                        RunStatus::Failed,
                        Some("the run stopped before producing anything".to_owned()),
                    );
                }
            }
        }
    }

    fn is_our_run(&self, ev: &TurnLifecycleEvent) -> bool {
        ev.session_id == self.session_id && ev.kind == TurnInputKind::IssueRun
    }

    /// The run's outcome from the store, or `None` if its turn has not
    /// finished yet.
    ///
    /// Takes the **newest** terminal issue turn, not the first: this
    /// session hosts every run its agent has worked on this issue, so the
    /// first one is that agent's first run forever. That is only
    /// unambiguous because an issue holds at most one unfinished run at a
    /// time.
    async fn reconcile(&self) -> Option<(RunStatus, Option<String>)> {
        let turns = match self.lifecycle.list_by_session(&self.session_id, None).await {
            Ok(turns) => turns,
            Err(e) => {
                warn!(run_id = %self.run.id, error = %e, "could not reconcile run via store");
                return None;
            }
        };
        turns
            .into_iter()
            .filter(|t| t.input_kind() == TurnInputKind::IssueRun && t.is_terminal())
            .max_by_key(|t| t.started_at)
            .map(|t| t.status.kind())
            .map(outcome_for)
    }
}

fn outcome_for(kind: TurnStatusKind) -> (RunStatus, Option<String>) {
    match kind {
        TurnStatusKind::Completed => (RunStatus::Done, None),
        TurnStatusKind::Cancelled => (RunStatus::Cancelled, None),
        TurnStatusKind::Failed => (
            RunStatus::Failed,
            Some("the run failed; its transcript has the detail".to_owned()),
        ),
        other => (
            RunStatus::Failed,
            Some(format!("the run ended as {other:?}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    //! What settling a run owes the card: a comment that arrived while it
    //! was executing has to *start* something, not merely be recorded — and
    //! which session a run is handed, which is what decides whose persona,
    //! skills and commit name it works under.

    use std::sync::Arc;

    use baybo_model::{
        AgentFramework, AgentHandle, AgentProfileId, ChannelType, MicroUsd, SessionId,
        TeamMembership,
    };
    use baybo_project::{NewIssueRequest, NewProject, ProjectManager};
    use baybo_store::project::{
        IssuePriority, IssueStatus, IssueUpdate, ProjectRow, ProjectUpdate, RunTrigger,
    };
    use baybo_workspace::WorkspacePaths;

    use super::*;
    use crate::actor::router::{ActorSpawner, LiveRateLimit, RouterConfig};
    use crate::actor::supervisor::AgentSupervisor;
    use crate::security::SecurityGateway;

    const HANDLE: &str = "dev-1";
    const OTHER_HANDLE: &str = "dev-2";
    const BOARD: &str = "Follow-ups";

    struct Board {
        projects: Arc<ProjectManager>,
        store: Arc<dyn ProjectStore>,
        /// The whole sqlite handle, for the stores this board's fixtures
        /// reach past `ProjectStore` for (the team roster, the router's
        /// profile lookups).
        db: baybo_storage::Store,
        project: ProjectRow,
        /// Every run the board handed to an executor, in order. A run
        /// written to the store that never lands here is a run nothing ever
        /// starts — and it holds the issue's only live-run slot.
        dispatched: Arc<parking_lot::Mutex<Vec<IssueRunRow>>>,
        _workspace: tempfile::TempDir,
    }

    impl Board {
        /// The run the board dispatched `nth` (0-based), which is how these
        /// tests get the row an executor would have been handed.
        fn nth_dispatched(&self, nth: usize) -> IssueRunRow {
            self.dispatched
                .lock()
                .get(nth)
                .cloned()
                .unwrap_or_else(|| panic!("the board dispatched a run #{nth}"))
        }

        /// Put `agent` on this board's team under `handle`.
        async fn hire(&self, agent: &AgentProfileId, handle: &str) {
            let now = chrono::Utc::now();
            self.db
                .agent_profile
                .create(&baybo_store::AgentProfileRow {
                    id: agent.clone(),
                    description: String::new(),
                    avatar_blob_id: None,
                    framework: AgentFramework::Baybo,
                    llm: None,
                    builtin: false,
                    team: Some(TeamMembership {
                        project_id: self.project.id.clone(),
                        handle: AgentHandle::parse(handle).expect("handle"),
                    }),
                    hired_by: None,
                    deleted_at: None,
                    created_at: now,
                    updated_at: now,
                })
                .await
                .expect("teammate");
        }

        /// Hand the card to somebody else, the way the assignee picker
        /// does. In progress and staffed either way, so this is a pure
        /// handover.
        async fn reassign(&self, to: &AgentProfileId) {
            self.projects
                .update_issue(
                    &self.project.id,
                    1,
                    IssueActor::User,
                    IssueUpdate {
                        assignee: Some(Some(to.clone())),
                        ..Default::default()
                    },
                )
                .await
                .expect("reassign");
        }
    }

    /// A board with one in-progress card assigned to `dev-1`, and its first
    /// run dispatched but not yet claimed.
    async fn board_with_in_progress_card() -> (Board, IssueRunRow) {
        let workspace = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(workspace.path().to_path_buf());
        tokio::fs::create_dir_all(paths.work_dir())
            .await
            .expect("work dir");
        let store = baybo_storage::Store::open(workspace.path().join("storage.db"))
            .await
            .expect("store");
        let dispatched: Arc<parking_lot::Mutex<Vec<IssueRunRow>>> = Arc::default();
        let projects = Arc::new(ProjectManager::new(
            Arc::clone(&store.project),
            Arc::clone(&store.agent_profile),
            paths,
            Arc::new(baybo_project::NoopProjectEvents),
            {
                let seen = Arc::clone(&dispatched);
                Arc::new(move |run| seen.lock().push(run))
            },
        ));
        let project = projects
            .create_project(NewProject {
                name: BOARD.to_owned(),
                description: String::new(),
                workdir: None,
                daily_budget: None,
            })
            .await
            .expect("project");

        let board = Board {
            projects,
            store: Arc::clone(&store.project),
            project,
            dispatched,
            db: store,
            _workspace: workspace,
        };

        // An assignee has to be on the board's team.
        let agent = AgentProfileId::parse(HANDLE).expect("agent id");
        board.hire(&agent, HANDLE).await;

        board
            .projects
            .create_issue(
                &board.project.id,
                IssueActor::User,
                NewIssueRequest {
                    title: "wire the importer".to_owned(),
                    description: String::new(),
                    status: IssueStatus::InProgress,
                    priority: IssuePriority::None,
                    assignee: Some(agent),
                    parent: None,
                    stage: 0,
                    source_key: None,
                },
            )
            .await
            .expect("issue");

        let first = board.nth_dispatched(0);
        (board, first)
    }

    /// A board with one in-progress card whose first run is executing —
    /// the state the waiter is in when somebody comments.
    async fn mid_run() -> (Board, IssueRunRow) {
        let (board, first) = board_with_in_progress_card().await;
        assert!(
            board
                .store
                .claim_run(&first.id, &SessionId::from("sess-issue-1"))
                .await
                .expect("claim"),
            "the executor took the first run"
        );
        (board, first)
    }

    /// A router wired to this board, plus the session store it mints into.
    ///
    /// These tests stop at the session a run would have been handed, which
    /// is the thing that decides whose persona, whose skills and whose name
    /// the work happens under — so the actor spawner is never reached.
    struct RouterHarness {
        router: Router,
        sessions: Arc<baybo_session::SessionManager>,
        _response_rx: tokio::sync::mpsc::Receiver<baybo_channels::AgentOutput>,
    }

    fn router_for(board: &Board) -> RouterHarness {
        let (response_tx, response_rx) = tokio::sync::mpsc::channel(8);
        let sessions = Arc::new(baybo_session::SessionManager::new(
            Arc::new(baybo_session::test_support::MemorySessionStore::new()),
            Arc::new(baybo_session::test_support::MemorySessionFolderStore::new()),
        ));
        let actor_spawner: ActorSpawner =
            Arc::new(move |_session, _llm, _model, _effort, _tx, _token| {
                crate::actor::mailbox::channel(1).0
            });
        let key = baybo_security::EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
            .expect("key");
        let (_trigger_tx, cron_trigger_rx) = tokio::sync::mpsc::channel(1);
        let router = Router::from_config(RouterConfig {
            session_manager: Arc::clone(&sessions),
            supervisor: AgentSupervisor::new(response_tx),
            channels: Arc::new(baybo_channels::ChannelRegistry::new()),
            security_gateway: Arc::new(SecurityGateway::new(
                Arc::new(baybo_security::LeakDetector::with_default_rules()),
                Arc::new(baybo_security::SecretVault::new(
                    key,
                    Arc::new(baybo_security::test_support::MemorySecretStore::new()),
                )),
            )),
            cost_manager: baybo_cost::CostManager::new(
                Arc::new(baybo_cost::test_support::MemoryCostStore::new()),
                std::collections::HashMap::new(),
                baybo_cost::SpendingLimits::default(),
            ),
            actor_spawner,
            turn_lifecycle: Arc::new(baybo_turn::TurnLifecycle::new(Arc::new(
                baybo_turn::test_support::MemoryTurnStore::new(),
            ))),
            cron_store: Arc::new(baybo_cron::test_support::InMemoryCronStore::new()),
            agent_profiles: Arc::clone(&board.db.agent_profile),
            cron_trigger_rx,
            issue_run_rx: None,
            board: Some(BoardWiring {
                store: Arc::clone(&board.store),
                events: Arc::new(baybo_project::NoopProjectEvents),
                manager: Arc::clone(&board.projects),
            }),
            actor_parent_token: CancellationToken::new(),
            rate_limit: LiveRateLimit::new(100, std::time::Duration::from_secs(60)),
            workspace: Arc::new(WorkspacePaths::new("/tmp/baybo-issue-router-test")),
        });
        RouterHarness {
            router,
            sessions,
            _response_rx: response_rx,
        }
    }

    fn run_event(run: &IssueRunRow) -> IssueRunEvent {
        IssueRunEvent {
            run: run.clone(),
            brief: "wire the importer".to_owned(),
            checkout: PathBuf::from("/tmp/does-not-matter"),
            user_id: "u1".to_owned(),
            channel: ChannelType::tui(),
        }
    }

    /// Say something on the card while its run is executing, and confirm
    /// the board deferred it rather than answering now.
    async fn comment_mid_run(board: &Board) {
        board
            .projects
            .comment(
                &board.project.id,
                1,
                IssueActor::User,
                "also handle the empty case",
            )
            .await
            .expect("comment");
        assert_eq!(
            board
                .projects
                .comment_delivery(&board.project.id, 1)
                .await
                .expect("delivery"),
            baybo_project::CommentDelivery::AfterCurrentRun,
            "the comment is deferred to whoever settles this run"
        );
        assert_eq!(
            board.dispatched.lock().len(),
            1,
            "and nothing was started while the card was busy"
        );
    }

    /// The deferred comment has to reach the *dispatcher*. A ledger row
    /// written straight to the store is a run nothing ever executes, and
    /// because an issue may hold only one live run it blocks every later
    /// start on that card until the next boot.
    #[tokio::test]
    async fn a_comment_left_mid_run_starts_a_follow_up_the_dispatcher_actually_gets() {
        let (board, first) = mid_run().await;
        comment_mid_run(&board).await;
        board
            .store
            .settle_run(&first.id, RunStatus::Done, None)
            .await
            .expect("settle");

        follow_up_on_comments(&board.projects, &board.store, &first, RunStatus::Done).await;

        let dispatched = board.dispatched.lock().clone();
        assert_eq!(dispatched.len(), 2, "the follow-up was handed to somebody");
        let follow_up = &dispatched[1];
        assert_eq!(follow_up.trigger, RunTrigger::Comment);
        assert_eq!(follow_up.number, first.number);
        assert_eq!(follow_up.attempt, 2);
        assert_eq!(
            follow_up.status,
            RunStatus::Queued,
            "queued and on its way, not parked"
        );
    }

    /// And it goes through the budget gate on the way, like every other
    /// start: a board with nothing left records the run it owes and holds
    /// it, rather than dispatching work it cannot pay for.
    #[tokio::test]
    async fn a_follow_up_the_board_cannot_afford_is_held_rather_than_dispatched() {
        let (board, first) = mid_run().await;
        comment_mid_run(&board).await;
        board
            .store
            .settle_run(&first.id, RunStatus::Done, None)
            .await
            .expect("settle");
        // Zero is how an operator pauses a board without archiving it.
        board
            .projects
            .update_project(
                &board.project.id,
                ProjectUpdate {
                    name: board.project.name.clone(),
                    description: board.project.description.clone(),
                    daily_budget: Some(MicroUsd::ZERO),
                },
            )
            .await
            .expect("budget");

        follow_up_on_comments(&board.projects, &board.store, &first, RunStatus::Done).await;

        assert_eq!(
            board.dispatched.lock().len(),
            1,
            "nothing started on a board with nothing left to spend"
        );
        let runs = board.store.list_runs(&first.issue_id).await.expect("runs");
        assert_eq!(runs.len(), 2, "but the run the comment asked for exists");
        assert_eq!(
            runs[0].status,
            RunStatus::Held,
            "owed and waiting for headroom, not dropped"
        );
    }

    /// Cancel is the operator saying stop. The card stays InProgress and
    /// stays assigned, so the board would happily answer "wake" — and a
    /// fresh run seconds after the Cancel button reads as the button not
    /// working.
    #[tokio::test]
    async fn cancelling_a_run_with_a_comment_waiting_does_not_start_another() {
        let (board, first) = mid_run().await;
        comment_mid_run(&board).await;
        board
            .store
            .settle_run(&first.id, RunStatus::Cancelled, None)
            .await
            .expect("settle");

        follow_up_on_comments(&board.projects, &board.store, &first, RunStatus::Cancelled).await;

        assert_eq!(
            board.dispatched.lock().len(),
            1,
            "cancelling a run must not immediately start another one"
        );
        assert_eq!(
            board
                .store
                .list_runs(&first.issue_id)
                .await
                .expect("runs")
                .len(),
            1,
            "and nothing was written that a boot sweep would later re-drive"
        );
    }

    /// A handover has to run as the agent the card now names.
    ///
    /// The session carries the binding, the binding selects the persona and
    /// the skills, and — since this PR — the name the run's commits are
    /// authored with. A run handed to the previous assignee's session does
    /// all of its work as that agent while the card, the timeline and the
    /// ledger row all say somebody else. The ledger row is right in that
    /// state, which is why asserting on it cannot see this.
    #[tokio::test]
    async fn a_reassigned_card_runs_as_its_new_agent_and_not_the_old_one() {
        let (board, first) = board_with_in_progress_card().await;
        let harness = router_for(&board);
        let dev_1 = AgentProfileId::parse(HANDLE).expect("agent id");
        let dev_2 = AgentProfileId::parse(OTHER_HANDLE).expect("agent id");
        board.hire(&dev_2, OTHER_HANDLE).await;

        let first_session = harness
            .router
            .issue_session(&board.store, &run_event(&first))
            .await
            .expect("session");
        assert_eq!(
            first_session.state.agent_id.as_ref(),
            Some(&dev_1),
            "the first run opens a session bound to the card's assignee"
        );
        board
            .store
            .claim_run(&first.id, &first_session.id)
            .await
            .expect("claim");
        board
            .store
            .settle_run(&first.id, RunStatus::Done, None)
            .await
            .expect("settle");

        board.reassign(&dev_2).await;
        let handover = board.nth_dispatched(1);
        assert_eq!(handover.agent_id, dev_2, "the run is dev-2's");
        let handover_session = harness
            .router
            .issue_session(&board.store, &run_event(&handover))
            .await
            .expect("session");
        assert_ne!(
            handover_session.id, first_session.id,
            "dev-2 must not work inside the session bound to dev-1"
        );
        assert_eq!(
            handover_session.state.agent_id.as_ref(),
            Some(&dev_2),
            "and the session it does work in is bound to dev-2"
        );

        // Continuity is per agent, not per run: hand the card back and the
        // first agent picks up where it left off rather than starting over.
        board
            .store
            .claim_run(&handover.id, &handover_session.id)
            .await
            .expect("claim");
        board
            .store
            .settle_run(&handover.id, RunStatus::Done, None)
            .await
            .expect("settle");
        board.reassign(&dev_1).await;
        let back = board.nth_dispatched(2);
        assert_eq!(back.agent_id, dev_1);
        assert_eq!(
            harness
                .router
                .issue_session(&board.store, &run_event(&back))
                .await
                .expect("session")
                .id,
            first_session.id,
            "an agent handed the card back continues in the session it already worked it in"
        );
    }

    /// The continuity F4 exists for, unchanged by the handover rule: a run
    /// interrupted by a restart is resumed in the session it was already
    /// running in, not a fresh one with an empty transcript.
    #[tokio::test]
    async fn a_resumed_run_continues_in_the_session_it_was_already_in() {
        let (board, first) = board_with_in_progress_card().await;
        let harness = router_for(&board);

        let opened = harness
            .router
            .issue_session(&board.store, &run_event(&first))
            .await
            .expect("session");
        board
            .store
            .claim_run(&first.id, &opened.id)
            .await
            .expect("claim");

        // What the boot sweep hands back: the same row, still unsettled.
        let resumed = harness
            .router
            .issue_session(&board.store, &run_event(&first))
            .await
            .expect("session");
        assert_eq!(resumed.id, opened.id);
        assert_eq!(
            harness
                .sessions
                .get(&opened.id)
                .await
                .expect("lookup")
                .map(|s| s.id),
            Some(opened.id),
            "and it is a session the manager actually holds"
        );
    }
}
