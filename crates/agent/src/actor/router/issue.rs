//! Executing a board issue's run.
//!
//! The ledger row already exists — `baybo-project` wrote it before anything
//! was told about it. This module is the consumer: it claims the row, mints
//! or reuses the issue's session, runs the brief as one turn, and settles
//! the row with what happened.
//!
//! The shape is cron's one-shot fire (`cron.rs`), with two differences that
//! matter. An issue keeps **one session across its runs**, so a follow-up
//! sees what the last one did; that is safe only because at most one run per
//! issue is ever in flight (a partial unique index enforces it), which is
//! what lets the waiter treat the terminal turn it sees as unambiguously its
//! own. And nothing is dispatched to a channel: an issue's audience is its
//! card.

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

impl Router {
    pub(super) async fn handle_issue_run(&mut self, event: IssueRunEvent) {
        let run_id = event.run.id.clone();
        // Both or neither: the store settles this run's row, the manager
        // starts whatever settling it turns out to owe. An assembly with a
        // board has both — this is the no-board case (the TUI's runtime).
        let (Some(store), Some(projects)) =
            (self.project_store.clone(), self.project_manager.clone())
        else {
            warn!(%run_id, "issue run arrived with no board wiring; cannot execute");
            return;
        };

        let session = match self.issue_session(&event).await {
            Ok(session) => session,
            Err(e) => {
                warn!(%run_id, error = %e, "could not open the issue's session");
                settle(
                    &store,
                    self.project_events.as_ref(),
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
            &store,
            self.project_events.as_ref(),
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
            store: Arc::clone(&store),
            projects: Arc::clone(&projects),
            events: self.project_events.clone(),
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

    /// The issue's session — reused across its runs, minted on the first.
    ///
    /// The binding is resolved from the run's assignee at mint time and is
    /// write-once thereafter, so reassigning an issue does not re-point an
    /// existing session; a new assignee's work happens under the agent the
    /// session was opened with until the issue's session is replaced.
    async fn issue_session(&self, event: &IssueRunEvent) -> anyhow::Result<Session> {
        let store = self
            .project_store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no project store"))?;
        let issue = store
            .get_issue(&event.run.project_id, event.run.number)
            .await?
            .ok_or_else(|| anyhow::anyhow!("issue #{} is gone", event.run.number))?;

        // A session that already ran this issue is the one to continue in.
        if let Some(previous) = store
            .list_runs(&issue.id)
            .await?
            .into_iter()
            .find_map(|run| run.session_id)
            && let Some(session) = self.session_manager.get(&previous).await?
        {
            return Ok(session);
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
async fn follow_up_on_comments(
    projects: &Arc<ProjectManager>,
    store: &Arc<dyn ProjectStore>,
    run: &IssueRunRow,
) {
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
    events: Option<&Arc<dyn ProjectEvents>>,
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
        Ok(true) => {
            if let Some(events) = events {
                events.board_changed(&issue.project_id, Some(issue.number));
            }
        }
        Ok(false) => {}
        Err(e) => warn!(run = %run.id, error = %e, "could not record the issue's branch"),
    }
}

async fn settle(
    store: &Arc<dyn ProjectStore>,
    events: Option<&Arc<dyn ProjectEvents>>,
    run: &IssueRunRow,
    status: RunStatus,
    error: Option<&str>,
) {
    match store.settle_run(&run.id, status, error).await {
        // Announce only a settle that actually landed: a replay of an
        // already-settled run changed nothing and has nothing to say —
        // and must not put a second entry on the timeline either.
        Ok(true) => {
            if let Some(events) = events {
                events.run_changed(&run.project_id, run.number);
            }
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
    events: Option<&Arc<dyn ProjectEvents>>,
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
        Ok(_) => {
            if let Some(events) = events {
                events.timeline_changed(&run.project_id, run.number);
            }
        }
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
    store: Arc<dyn ProjectStore>,
    /// The board, for the one thing settling a run can owe: a follow-up for
    /// a comment that landed mid-run. Held as the manager rather than as a
    /// store handle so that follow-up goes through the same enqueue — and
    /// therefore the same budget gate and the same dispatcher — as every
    /// other run.
    projects: Arc<ProjectManager>,
    events: Option<Arc<dyn ProjectEvents>>,
}

impl IssueRunWaiter {
    async fn run(mut self, actor_token: CancellationToken) {
        let (status, error) = self.await_run(actor_token).await;
        let run_id = &self.run.id;
        info!(%run_id, ?status, "issue run settled");
        settle(
            &self.store,
            self.events.as_ref(),
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
        follow_up_on_comments(&self.projects, &self.store, &self.run).await;
        surface_branch(&self.store, self.events.as_ref(), &self.checkout, &self.run).await;
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
    /// session hosts every run of its issue, so the first one is run #1's
    /// forever. That is only unambiguous because an issue holds at most one
    /// unfinished run at a time.
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
    //! was executing has to *start* something, not merely be recorded.

    use std::sync::Arc;

    use baybo_model::{
        AgentFramework, AgentHandle, AgentProfileId, MicroUsd, SessionId, TeamMembership,
    };
    use baybo_project::{NewIssueRequest, NewProject, ProjectManager};
    use baybo_store::project::{IssuePriority, IssueStatus, ProjectRow, ProjectUpdate, RunTrigger};
    use baybo_workspace::WorkspacePaths;

    use super::*;

    const HANDLE: &str = "dev-1";
    const BOARD: &str = "Follow-ups";

    struct Board {
        projects: Arc<ProjectManager>,
        store: Arc<dyn ProjectStore>,
        project: ProjectRow,
        /// Every run the board handed to an executor, in order. A run
        /// written to the store that never lands here is a run nothing ever
        /// starts — and it holds the issue's only live-run slot.
        dispatched: Arc<parking_lot::Mutex<Vec<IssueRunRow>>>,
        _workspace: tempfile::TempDir,
    }

    /// A board with one in-progress card whose first run is executing —
    /// the state the waiter is in when somebody comments.
    async fn mid_run() -> (Board, IssueRunRow) {
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

        // An assignee has to be on the board's team.
        let agent = AgentProfileId::parse(HANDLE).expect("agent id");
        let now = chrono::Utc::now();
        store
            .agent_profile
            .create(&baybo_store::AgentProfileRow {
                id: agent.clone(),
                description: String::new(),
                avatar_blob_id: None,
                framework: AgentFramework::Baybo,
                llm: None,
                builtin: false,
                team: Some(TeamMembership {
                    project_id: project.id.clone(),
                    handle: AgentHandle::parse(HANDLE).expect("handle"),
                }),
                hired_by: None,
                deleted_at: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("teammate");

        projects
            .create_issue(
                &project.id,
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

        let first = dispatched.lock().first().cloned().expect("first run");
        assert!(
            store
                .project
                .claim_run(&first.id, &SessionId::from("sess-issue-1"))
                .await
                .expect("claim"),
            "the executor took the first run"
        );

        (
            Board {
                projects,
                store: Arc::clone(&store.project),
                project,
                dispatched,
                _workspace: workspace,
            },
            first,
        )
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

        follow_up_on_comments(&board.projects, &board.store, &first).await;

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

        follow_up_on_comments(&board.projects, &board.store, &first).await;

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
}
