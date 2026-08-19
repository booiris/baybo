//! Executing a board issue's run.

use std::path::PathBuf;
use std::sync::Arc;

use baybo_model::{AgentBinding, AgentProfileId, Session, SessionId, TriggerSource, TurnId, User};
use baybo_project::{IssueRunEvent, ProjectManager, RunOutcome, session_run_to_continue};
use baybo_store::project::{IssueRunRow, RunStatus};
use baybo_turn::{
    CancelReason, TurnInputKind, TurnLifecycle, TurnLifecycleEvent, TurnStatus, TurnStatusKind,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::actor::AgentMessage;

use super::Router;

impl Router {
    pub(super) async fn handle_issue_run(&mut self, event: IssueRunEvent) {
        let run_id = event.run.id.clone();
        let Some(board) = self.board.clone() else {
            warn!(%run_id, "issue run arrived with no board; cannot execute");
            return;
        };

        let session = match self.issue_session(&board, &event).await {
            Ok(session) => session,
            Err(e) => {
                warn!(%run_id, error = %e, "could not open the issue's session");
                board
                    .finish_run(
                        &event.run,
                        &event.checkout,
                        event.briefed_at,
                        RunOutcome {
                            status: RunStatus::Failed,
                            error: Some(e.to_string()),
                            stopped_by_a_human: false,
                        },
                    )
                    .await;
                return;
            }
        };

        match board.start_run(&event.run, &session.id).await {
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

        // Subscribed before the trigger is sent, and this is load-bearing:
        // the terminal event is published from inside the run's own turn, so
        // a subscription opened afterwards can miss a fast failure entirely.
        let waiter = IssueRunWaiter {
            enqueued: event.run.clone(),
            checkout: checkout.clone(),
            briefed_at: event.briefed_at,
            session_id: session.id.clone(),
            lifecycle: Arc::clone(&self.turn_lifecycle),
            terminal_rx: self.turn_lifecycle.subscribe_lifecycle_events(),
            board: Arc::clone(&board),
            shutdown: self.actor_parent_token.clone(),
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
            files: event.files.clone(),
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

    async fn issue_session(
        &self,
        board: &Arc<ProjectManager>,
        event: &IssueRunEvent,
    ) -> anyhow::Result<Session> {
        let issue = board
            .get_issue(&event.run.project_id, event.run.number)
            .await?;

        // Before either branch, not only the minting one: a session opened
        // while the agent was on baybo must not be handed a run the agent
        // can no longer host.
        let binding = self.binding_for(&event.run.agent_id).await?;

        let runs = board
            .list_runs(&event.run.project_id, event.run.number)
            .await?;
        if let Some(previous) = session_run_to_continue(&event.run, &runs)
            && let Some(id) = previous.session_id.as_ref()
            && let Some(session) = self.session_manager.get(id).await?
            && session.state.agent_id_or_builtin() == event.run.agent_id
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
                Some(binding),
            )
            .await?)
    }

    async fn binding_for(&self, agent: &AgentProfileId) -> anyhow::Result<AgentBinding> {
        let profile = self
            .agent_profiles
            .get(agent)
            .await?
            .ok_or_else(|| anyhow::anyhow!("agent {agent} is gone"))?;
        if !baybo_project::can_host_a_session(profile.framework) {
            anyhow::bail!(
                "{agent} runs on {}, which cannot yet host an issue's session",
                profile.framework.as_str()
            );
        }
        Ok(AgentBinding {
            agent_id: agent.clone(),
            framework: profile.framework,
        })
    }
}

struct IssueRunWaiter {
    /// The run as it was **enqueued** — the row the dispatcher handed over,
    /// cloned before the claim stamped it. Its `status` therefore reads
    /// `Queued` and its `started_at` is empty for the whole life of the
    /// turn, however long that is. Only what never changes is read from it
    /// here — the ids, the number, `created_at` — and the board re-reads
    /// the row for anything the claim wrote.
    enqueued: IssueRunRow,
    /// The worktree this run worked in. Handed to the board when the run is
    /// over, which is when the branch it left behind can be named.
    checkout: PathBuf,
    /// When the brief this run was handed was read off the card. Carried
    /// rather than re-derived because it is the only record of it — the
    /// ledger row's two instants both fall on the wrong side of the brief.
    briefed_at: chrono::DateTime<chrono::Utc>,
    session_id: SessionId,
    lifecycle: Arc<TurnLifecycle>,
    terminal_rx: broadcast::Receiver<TurnLifecycleEvent>,
    /// The board this run belongs to. The manager and nothing under it: how
    /// a settled run is written down, and whether it owes a follow-up, are
    /// the board's rules, and this crate only reports what it watched.
    board: Arc<ProjectManager>,
    /// The process-wide parent token, kept apart from the actor's own: a
    /// finished one-shot actor cancels *its* token too, so that firing says
    /// nothing about why. This one only fires when the process is going
    /// down — and an unfinished run must then stay unsettled, because any
    /// terminal status written here would take the row out of reach of the
    /// boot sweep (`requeue_unsettled`) that exists to hand it back out.
    shutdown: CancellationToken,
}

impl IssueRunWaiter {
    async fn run(mut self, actor_token: CancellationToken) {
        let Some(outcome) = self.await_run(actor_token).await else {
            info!(
                run_id = %self.enqueued.id,
                "issue run interrupted by shutdown; leaving it for the boot resume sweep"
            );
            return;
        };
        info!(run_id = %self.enqueued.id, status = ?outcome.status, "issue run settled");
        self.board
            .finish_run(&self.enqueued, &self.checkout, self.briefed_at, outcome)
            .await;
    }

    /// `None` is not an outcome: it says the process shut down under the run
    /// and the row is the boot sweep's to requeue, so nothing gets settled.
    async fn await_run(&mut self, actor_token: CancellationToken) -> Option<RunOutcome> {
        loop {
            tokio::select! {
                event = self.terminal_rx.recv() => match event {
                    Ok(ev) if self.is_our_run(&ev) => {
                        let Some(kind) = ev.phase.terminal_status() else {
                            continue;
                        };
                        let outcome = self.outcome_of_edge(&ev.turn_id, kind).await;
                        return self.unless_shutdown_interrupted_it(outcome);
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            run_id = %self.enqueued.id,
                            skipped = n,
                            "issue waiter lagged on the lifecycle bus; reconciling via store"
                        );
                        if let Some(outcome) = self.reconcile().await {
                            return self.unless_shutdown_interrupted_it(outcome);
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        if self.shutdown.is_cancelled() {
                            return None;
                        }
                        return Some(failed_outcome("lifecycle bus closed before the run finished"));
                    }
                },
                _ = actor_token.cancelled() => {
                    // The actor is gone: it either finished and we have not
                    // drained its event, or it died before opening a turn.
                    // The store says which.
                    if let Some(outcome) = self.reconcile().await {
                        return self.unless_shutdown_interrupted_it(outcome);
                    }
                    if self.shutdown.is_cancelled() {
                        return None;
                    }
                    // The board owns whether work reached the card; the branch may still differ.
                    let left_a_mark = self
                        .board
                        .run_left_a_mark(&self.enqueued, self.briefed_at)
                        .await;
                    return Some(failed_outcome(if left_a_mark {
                        RUN_ENDED_MID_FLIGHT
                    } else {
                        RUN_LEFT_NOTHING_ON_THE_CARD
                    }));
                }
            }
        }
    }

    /// A completed or failed turn ended on its own and settles no matter
    /// what; so does a cancel a human asked for — that was a decision, not
    /// an interruption. But a *system* cancel observed while the process is
    /// shutting down is the shutdown's own doing, and writing it down would
    /// turn "restart the daemon" into "every in-flight run ends". Those runs
    /// are left unsettled for the boot sweep instead.
    fn unless_shutdown_interrupted_it(&self, outcome: RunOutcome) -> Option<RunOutcome> {
        let interrupted = outcome.status == RunStatus::Cancelled
            && !outcome.stopped_by_a_human
            && self.shutdown.is_cancelled();
        (!interrupted).then_some(outcome)
    }

    fn is_our_run(&self, ev: &TurnLifecycleEvent) -> bool {
        ev.session_id == self.session_id && ev.kind == TurnInputKind::IssueRun
    }

    /// Preserve failure and system-cancellation reasons on the card.
    async fn outcome_of_edge(&self, turn: &TurnId, kind: TurnStatusKind) -> RunOutcome {
        match self.lifecycle.get(turn).await {
            Ok(Some(turn)) => outcome_of_status(&turn.status),
            Ok(None) => outcome_of(kind),
            Err(e) => {
                warn!(run_id = %self.enqueued.id, error = %e, "could not read how the run's turn ended");
                outcome_of(kind)
            }
        }
    }

    async fn reconcile(&self) -> Option<RunOutcome> {
        let turns = match self.lifecycle.list_by_session(&self.session_id, None).await {
            Ok(turns) => turns,
            Err(e) => {
                warn!(run_id = %self.enqueued.id, error = %e, "could not reconcile run via store");
                return None;
            }
        };
        turns
            .into_iter()
            .filter(|t| t.input_kind() == TurnInputKind::IssueRun && t.is_terminal())
            .filter(|t| t.created_at >= self.enqueued.created_at)
            .max_by_key(|t| t.created_at)
            .map(|t| outcome_of_status(&t.status))
    }
}

/// Used when no run activity reached the card; the branch may still contain work.
const RUN_LEFT_NOTHING_ON_THE_CARD: &str = r#"the run stopped before it finished, and nothing it did reached this card — its branch may still hold work, so check there before concluding it did none"#;

/// Used when the card already records run activity.
const RUN_ENDED_MID_FLIGHT: &str = r#"the run stopped before it finished, after it had already worked the card — what it did is above"#;

/// Maximum provider error length stored on a card.
const MAX_RUN_ERROR_CHARS: usize = 400;

fn clip(reason: &str) -> String {
    let mut clipped: String = reason.chars().take(MAX_RUN_ERROR_CHARS).collect();
    if reason.chars().nth(MAX_RUN_ERROR_CHARS).is_some() {
        clipped.push('…');
    }
    clipped
}

/// Map the stored terminal status to the card's run outcome.
fn outcome_of_status(status: &TurnStatus) -> RunOutcome {
    match status {
        TurnStatus::Failed { reason } => failed_outcome(clip(reason)),
        TurnStatus::Cancelled { reason, .. } => RunOutcome {
            status: RunStatus::Cancelled,
            error: cancel_note(*reason).map(str::to_owned),
            stopped_by_a_human: matches!(
                reason,
                CancelReason::OperatorCancel | CancelReason::UserStopped
            ),
        },
        other => outcome_of(other.kind()),
    }
}

/// Explain system cancellations; user-requested stops need no note.
fn cancel_note(reason: CancelReason) -> Option<&'static str> {
    match reason {
        CancelReason::OperatorCancel | CancelReason::UserStopped => None,
        CancelReason::SystemCrash => Some("the process running this turn went down under it"),
        CancelReason::SubagentTimeout => Some("a subagent it was waiting on hit its idle timeout"),
        CancelReason::ParentCancelled => Some("the session it ran under was cancelled"),
        CancelReason::ParentDeleted => Some("the session it ran under was deleted"),
        CancelReason::UserPreempt => {
            Some("something else was sent into its session while it was working")
        }
    }
}

/// How a turn ending maps onto how a run ended. The only half of a
/// [`RunOutcome`] this crate is entitled to decide: it is the one that
/// watched the turn, and the board is the one that decides what the answer
/// costs the card.
fn outcome_of(kind: TurnStatusKind) -> RunOutcome {
    match kind {
        TurnStatusKind::Completed => RunOutcome {
            status: RunStatus::Done,
            error: None,
            stopped_by_a_human: false,
        },
        TurnStatusKind::Cancelled => RunOutcome {
            status: RunStatus::Cancelled,
            error: None,
            stopped_by_a_human: false,
        },
        TurnStatusKind::Failed => failed_outcome("the run failed; its transcript has the detail"),
        other => failed_outcome(format!("the run ended as {other:?}")),
    }
}

fn failed_outcome(error: impl Into<String>) -> RunOutcome {
    RunOutcome {
        status: RunStatus::Failed,
        error: Some(error.into()),
        stopped_by_a_human: false,
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use baybo_model::{
        AgentFramework, AgentHandle, AgentProfileId, ChannelType, MicroUsd, SessionId,
        TeamMembership,
    };
    use baybo_project::{NewIssueRequest, NewProject, ProjectManager, session_run_before};
    use baybo_store::project::{
        DEFAULT_MAX_PARALLEL_ISSUE_RUNS, IssueActor, IssuePriority, IssueStatus, IssueUpdate,
        ProjectRow, ProjectStore, ProjectUpdate, RunTrigger,
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
        db: baybo_storage::Store,
        project: ProjectRow,
        dispatched: Arc<parking_lot::Mutex<Vec<IssueRunRow>>>,
        _workspace: tempfile::TempDir,
    }

    impl Board {
        fn nth_dispatched(&self, nth: usize) -> IssueRunRow {
            self.dispatched
                .lock()
                .get(nth)
                .cloned()
                .unwrap_or_else(|| panic!("the board dispatched a run #{nth}"))
        }

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
                    None,
                )
                .await
                .expect("reassign");
        }
    }

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
            Arc::clone(&store.blob),
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
                daily_budget_tokens: None,
                max_parallel_issue_runs: None,
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
                    attachments: Vec::new(),
                    status: IssueStatus::InProgress,
                    priority: IssuePriority::None,
                    assignee: Some(agent),
                    parent: None,
                    stage: 0,
                    source_key: None,
                    filed_from: None,
                },
            )
            .await
            .expect("issue");

        let first = board.nth_dispatched(0);
        (board, first)
    }

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
            board: Some(Arc::clone(&board.projects)),
            actor_parent_token: CancellationToken::new(),
            rate_limit: LiveRateLimit::new(100, std::time::Duration::from_secs(60)),
            workspace: Arc::new(WorkspacePaths::new("/tmp/baybo-issue-router-test")),
            inbound_dedup: Arc::new(baybo_channels::InboundDedup::new()),
        });
        RouterHarness {
            router,
            sessions,
            _response_rx: response_rx,
        }
    }

    fn run_event(run: &IssueRunRow) -> IssueRunEvent {
        IssueRunEvent {
            briefed_at: run.created_at,
            run: run.clone(),
            brief: "wire the importer".to_owned(),
            files: Vec::new(),
            checkout: PathBuf::from("/tmp/does-not-matter"),
            user_id: "u1".to_owned(),
            channel: ChannelType::tui(),
        }
    }

    /// Close a run out the way the waiter does. The checkout is a path that
    /// never existed — these tests are about what settling owes the card,
    /// and a card with no worktree has no branch to surface.
    async fn finish(board: &Board, run: &IssueRunRow, outcome: RunOutcome) {
        board
            .projects
            .finish_run(
                run,
                std::path::Path::new("/nonexistent/checkout"),
                run.created_at,
                outcome,
            )
            .await;
    }

    async fn comment_mid_run(board: &Board) {
        board
            .projects
            .comment(
                &board.project.id,
                1,
                IssueActor::User,
                "also handle the empty case",
                &[],
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

    #[tokio::test]
    async fn a_comment_left_mid_run_starts_a_follow_up_the_dispatcher_actually_gets() {
        let (board, first) = mid_run().await;
        comment_mid_run(&board).await;
        finish(&board, &first, outcome_of(TurnStatusKind::Completed)).await;

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

    #[tokio::test]
    async fn a_follow_up_the_board_cannot_afford_is_held_rather_than_dispatched() {
        let (board, first) = mid_run().await;
        comment_mid_run(&board).await;
        board
            .projects
            .update_project(
                &board.project.id,
                ProjectUpdate {
                    name: board.project.name.clone(),
                    description: board.project.description.clone(),
                    daily_budget: Some(MicroUsd::ZERO),
                    daily_budget_tokens: None,
                    max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                },
            )
            .await
            .expect("budget");

        finish(&board, &first, outcome_of(TurnStatusKind::Completed)).await;

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

    async fn outcome_after_cancel(
        board: &Board,
        run: &IssueRunRow,
        session: &SessionId,
        reason: CancelReason,
    ) -> RunOutcome {
        let lifecycle = Arc::new(TurnLifecycle::new(Arc::new(
            baybo_turn::test_support::MemoryTurnStore::new(),
        )));
        let turn = lifecycle
            .start_turn(
                session.clone(),
                baybo_model::TriggerKind::Issue,
                baybo_turn::TurnInput::IssueRun {
                    run_id: run.id.clone(),
                    brief: Vec::new(),
                },
                None,
            )
            .await
            .expect("turn");
        lifecycle.start(&turn.id).await.expect("start");
        lifecycle
            .cancel(&turn.id, reason, Vec::new())
            .await
            .expect("cancel");

        let waiter = IssueRunWaiter {
            enqueued: run.clone(),
            briefed_at: run.created_at,
            checkout: PathBuf::from("/tmp/does-not-matter"),
            session_id: session.clone(),
            terminal_rx: lifecycle.subscribe_lifecycle_events(),
            lifecycle: Arc::clone(&lifecycle),
            board: Arc::clone(&board.projects),
            shutdown: CancellationToken::new(),
        };
        let reconciled = waiter.reconcile().await.expect("the turn is terminal");
        assert_eq!(
            waiter
                .outcome_of_edge(&turn.id, TurnStatusKind::Cancelled)
                .await,
            reconciled,
            "the bus edge and the store must read the same cancel the same way"
        );
        reconciled
    }

    #[tokio::test]
    async fn cancelling_a_run_with_a_comment_waiting_does_not_start_another() {
        let (board, first) = mid_run().await;
        comment_mid_run(&board).await;
        let outcome = outcome_after_cancel(
            &board,
            &first,
            &SessionId::from("sess-issue-1"),
            CancelReason::OperatorCancel,
        )
        .await;
        assert_eq!(outcome.status, RunStatus::Cancelled);
        finish(&board, &first, outcome).await;

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

    #[tokio::test]
    async fn a_run_that_died_with_a_comment_waiting_still_starts_a_follow_up() {
        for reason in [CancelReason::SystemCrash, CancelReason::ParentCancelled] {
            let (board, first) = mid_run().await;
            comment_mid_run(&board).await;
            let outcome =
                outcome_after_cancel(&board, &first, &SessionId::from("sess-issue-1"), reason)
                    .await;
            assert_eq!(
                outcome.status,
                RunStatus::Cancelled,
                "{reason:?} settles the row exactly as a Stop does"
            );
            finish(&board, &first, outcome).await;

            let dispatched = board.dispatched.lock().clone();
            assert_eq!(
                dispatched.len(),
                2,
                "a run that died on {reason:?} must not swallow the comment"
            );
            assert_eq!(dispatched[1].trigger, RunTrigger::Comment);
        }
    }

    #[tokio::test]
    async fn a_reassigned_card_runs_as_its_new_agent_and_not_the_old_one() {
        let (board, first) = board_with_in_progress_card().await;
        let harness = router_for(&board);
        let dev_1 = AgentProfileId::parse(HANDLE).expect("agent id");
        let dev_2 = AgentProfileId::parse(OTHER_HANDLE).expect("agent id");
        board.hire(&dev_2, OTHER_HANDLE).await;

        let first_session = harness
            .router
            .issue_session(&board.projects, &run_event(&first))
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
            .issue_session(&board.projects, &run_event(&handover))
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
            window_and_session_agree(&board, &harness, &back).await.id,
            first_session.id,
            "an agent handed the card back continues in the session it already worked it in"
        );
    }

    async fn window_and_session_agree(
        board: &Board,
        harness: &RouterHarness,
        run: &IssueRunRow,
    ) -> Session {
        let runs = board.store.list_runs(&run.issue_id).await.expect("runs");
        let session = harness
            .router
            .issue_session(&board.projects, &run_event(run))
            .await
            .expect("session");
        match session_run_before(run, &runs) {
            Some(previous) => assert_eq!(
                previous.session_id.as_ref(),
                Some(&session.id),
                "the window names a run whose session this one is not given"
            ),
            None => assert!(
                runs.iter()
                    .all(|other| other.session_id.as_ref() != Some(&session.id)),
                "the window says this run has read nothing, and the router handed it a transcript that has"
            ),
        }
        session
    }

    #[tokio::test]
    async fn a_run_whose_predecessor_never_opened_a_session_starts_a_fresh_one() {
        let (board, first) = board_with_in_progress_card().await;
        let harness = router_for(&board);

        assert!(
            board
                .projects
                .cancel_run(&board.project.id, 1)
                .await
                .expect("cancel")
                .is_none(),
            "a queued run is settled where it stands; there is no session to stop"
        );
        board
            .projects
            .comment(
                &board.project.id,
                1,
                IssueActor::User,
                "start with the CSV path",
                &[],
            )
            .await
            .expect("comment");

        let second = board.nth_dispatched(1);
        assert_eq!(
            second.agent_id, first.agent_id,
            "the same agent is asked again"
        );
        let session = window_and_session_agree(&board, &harness, &second).await;
        let runs = board.store.list_runs(&second.issue_id).await.expect("runs");
        assert!(
            session_run_before(&second, &runs).is_none(),
            "there is no previous run for its brief to be a delta from"
        );
        assert_eq!(
            harness
                .router
                .turn_lifecycle
                .list_by_session(&session.id, None)
                .await
                .expect("turns")
                .len(),
            0,
            "and the transcript it opens is empty"
        );
    }

    #[tokio::test]
    async fn a_run_whose_actor_never_opened_a_turn_does_not_inherit_the_last_ones_outcome() {
        let (board, first) = mid_run().await;
        let session = SessionId::from("sess-issue-1");
        let lifecycle = Arc::new(TurnLifecycle::new(Arc::new(
            baybo_turn::test_support::MemoryTurnStore::new(),
        )));

        let stopped = lifecycle
            .start_turn(
                session.clone(),
                baybo_model::TriggerKind::Issue,
                baybo_turn::TurnInput::IssueRun {
                    run_id: first.id.clone(),
                    brief: Vec::new(),
                },
                None,
            )
            .await
            .expect("turn");
        lifecycle.start(&stopped.id).await.expect("start");
        lifecycle
            .cancel(&stopped.id, CancelReason::OperatorCancel, Vec::new())
            .await
            .expect("cancel");
        board
            .store
            .settle_run(&first.id, RunStatus::Cancelled, None)
            .await
            .expect("settle");

        board
            .projects
            .comment(
                &board.project.id,
                1,
                IssueActor::User,
                "start with the CSV path",
                &[],
            )
            .await
            .expect("comment");
        let second = board.nth_dispatched(1);
        assert!(
            stopped.created_at < second.created_at,
            "the run being waited on has to be the newer of the two for this to mean anything"
        );
        board
            .store
            .claim_run(&second.id, &session)
            .await
            .expect("claim");
        board
            .projects
            .comment(
                &board.project.id,
                1,
                IssueActor::User,
                "also handle the empty case",
                &[],
            )
            .await
            .expect("comment");

        let mut waiter = IssueRunWaiter {
            briefed_at: second.created_at,
            enqueued: second.clone(),
            checkout: PathBuf::from("/tmp/does-not-matter"),
            session_id: session.clone(),
            terminal_rx: lifecycle.subscribe_lifecycle_events(),
            lifecycle: Arc::clone(&lifecycle),
            board: Arc::clone(&board.projects),
            shutdown: CancellationToken::new(),
        };
        assert_eq!(
            waiter.reconcile().await,
            None,
            "run #1's ending is not run #2's outcome"
        );

        let actor_token = CancellationToken::new();
        actor_token.cancel();
        let outcome = waiter
            .await_run(actor_token)
            .await
            .expect("a lone actor death is an ending, not a shutdown");
        assert_eq!(
            outcome.status,
            RunStatus::Failed,
            "a run that produced nothing failed; it was not cancelled"
        );
        assert!(
            !outcome.stopped_by_a_human,
            "and nobody stopped it — the Cancel it would have inherited was aimed at run #1"
        );

        finish(&board, &second, outcome).await;
        let dispatched = board.dispatched.lock().clone();
        assert_eq!(
            dispatched.len(),
            3,
            "so the comment left during run #2 still starts something"
        );
        assert_eq!(dispatched[2].trigger, RunTrigger::Comment);
    }

    /// Opens the run's turn and leaves it mid-flight, the state a shutdown
    /// finds a live run in.
    async fn open_turn(
        lifecycle: &TurnLifecycle,
        run: &IssueRunRow,
        session: &SessionId,
    ) -> TurnId {
        let turn = lifecycle
            .start_turn(
                session.clone(),
                baybo_model::TriggerKind::Issue,
                baybo_turn::TurnInput::IssueRun {
                    run_id: run.id.clone(),
                    brief: Vec::new(),
                },
                None,
            )
            .await
            .expect("turn");
        lifecycle.start(&turn.id).await.expect("start");
        turn.id
    }

    fn waiter_for(
        board: &Board,
        run: &IssueRunRow,
        session: &SessionId,
        lifecycle: &Arc<TurnLifecycle>,
        shutdown: &CancellationToken,
    ) -> IssueRunWaiter {
        IssueRunWaiter {
            enqueued: run.clone(),
            briefed_at: run.created_at,
            checkout: PathBuf::from("/tmp/does-not-matter"),
            session_id: session.clone(),
            terminal_rx: lifecycle.subscribe_lifecycle_events(),
            lifecycle: Arc::clone(lifecycle),
            board: Arc::clone(&board.projects),
            shutdown: shutdown.clone(),
        }
    }

    fn cancelled_token() -> CancellationToken {
        let token = CancellationToken::new();
        token.cancel();
        token
    }

    #[tokio::test]
    async fn a_shutdown_mid_run_leaves_the_row_for_the_boot_sweep() {
        let (board, first) = mid_run().await;
        let session = SessionId::from("sess-issue-1");
        let lifecycle = Arc::new(TurnLifecycle::new(Arc::new(
            baybo_turn::test_support::MemoryTurnStore::new(),
        )));
        open_turn(&lifecycle, &first, &session).await;

        let shutdown = cancelled_token();
        let mut waiter = waiter_for(&board, &first, &session, &lifecycle, &shutdown);
        assert_eq!(
            waiter.await_run(cancelled_token()).await,
            None,
            "a run the shutdown caught mid-turn settles nothing"
        );
        let runs = board.store.list_runs(&first.issue_id).await.expect("runs");
        assert!(
            runs.iter()
                .any(|r| r.id == first.id && r.settled_at.is_none()),
            "the row stays unsettled, which is what the boot sweep requeues"
        );
    }

    #[tokio::test]
    async fn a_shutdown_cancelled_turn_is_an_interruption_not_an_ending() {
        let (board, first) = mid_run().await;
        let session = SessionId::from("sess-issue-1");
        let lifecycle = Arc::new(TurnLifecycle::new(Arc::new(
            baybo_turn::test_support::MemoryTurnStore::new(),
        )));
        let turn = open_turn(&lifecycle, &first, &session).await;

        // The shutdown's own cancel landed before the waiter looked: same
        // restart, only the race resolved the other way.
        lifecycle
            .cancel(&turn, CancelReason::ParentCancelled, Vec::new())
            .await
            .expect("cancel");

        let shutdown = cancelled_token();
        let mut waiter = waiter_for(&board, &first, &session, &lifecycle, &shutdown);
        assert_eq!(
            waiter.await_run(cancelled_token()).await,
            None,
            "the shutdown cancelling the turn itself is not an ending either"
        );
    }

    #[tokio::test]
    async fn a_human_cancel_settles_even_while_the_process_shuts_down() {
        let (board, first) = mid_run().await;
        let session = SessionId::from("sess-issue-1");
        let lifecycle = Arc::new(TurnLifecycle::new(Arc::new(
            baybo_turn::test_support::MemoryTurnStore::new(),
        )));
        let turn = open_turn(&lifecycle, &first, &session).await;
        lifecycle
            .cancel(&turn, CancelReason::OperatorCancel, Vec::new())
            .await
            .expect("cancel");

        let shutdown = cancelled_token();
        let mut waiter = waiter_for(&board, &first, &session, &lifecycle, &shutdown);
        let outcome = waiter
            .await_run(cancelled_token())
            .await
            .expect("a cancel the operator asked for is a decision, and it sticks");
        assert_eq!(outcome.status, RunStatus::Cancelled);
        assert!(outcome.stopped_by_a_human);
    }

    #[tokio::test]
    async fn a_resumed_run_continues_in_the_session_it_was_already_in() {
        let (board, first) = board_with_in_progress_card().await;
        let harness = router_for(&board);

        let opened = harness
            .router
            .issue_session(&board.projects, &run_event(&first))
            .await
            .expect("session");
        board
            .store
            .claim_run(&first.id, &opened.id)
            .await
            .expect("claim");

        let resumed = harness
            .router
            .issue_session(&board.projects, &run_event(&first))
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

    #[tokio::test]
    async fn a_run_is_refused_a_session_its_agent_can_no_longer_host() {
        let (board, run) = board_with_in_progress_card().await;
        let harness = router_for(&board);
        let agent = AgentProfileId::parse(HANDLE).expect("agent id");

        let before = harness
            .router
            .binding_for(&agent)
            .await
            .expect("baybo hosts a session");
        assert_eq!(before.framework, AgentFramework::Baybo);

        board
            .db
            .agent_profile
            .update(
                &agent,
                &baybo_store::AgentProfileUpdate {
                    description: String::new(),
                    framework: AgentFramework::Codex,
                },
            )
            .await
            .expect("the operator moves dev-1 to codex");

        let refused = harness
            .router
            .issue_session(&board.projects, &run_event(&run))
            .await
            .expect_err("codex cannot host an issue's session");
        assert!(
            refused
                .to_string()
                .contains("cannot yet host an issue's session"),
            "and it says why, because this reason lands on the card: {refused}"
        );
        assert!(
            board
                .store
                .list_runs(&run.issue_id)
                .await
                .expect("runs")
                .iter()
                .all(|row| row.session_id.is_none()),
            "no session was minted for it"
        );
    }

    #[test]
    fn every_cancel_reason_a_person_did_not_ask_for_says_why() {
        for reason in [
            CancelReason::UserPreempt,
            CancelReason::SystemCrash,
            CancelReason::SubagentTimeout,
            CancelReason::ParentCancelled,
            CancelReason::ParentDeleted,
            CancelReason::OperatorCancel,
            CancelReason::UserStopped,
        ] {
            let outcome = outcome_of_status(&TurnStatus::Cancelled {
                reason,
                partial_artifacts: Vec::new(),
            });
            assert_eq!(outcome.status, RunStatus::Cancelled);
            assert_eq!(
                outcome.error.is_some(),
                !outcome.stopped_by_a_human,
                "a person who pressed stop knows why; everything else used to reach the card as a \
                 bare `cancelled`, which is the blank a lead fills in with a guess: {reason:?}"
            );
        }
    }

    #[test]
    fn a_providers_failure_text_reaches_the_card_clipped() {
        let outcome = outcome_of_status(&TurnStatus::Failed {
            reason: "x".repeat(5_000),
        });
        assert_eq!(outcome.status, RunStatus::Failed);
        let said = outcome
            .error
            .expect("the card is told what the provider said");
        assert_eq!(said.chars().count(), MAX_RUN_ERROR_CHARS + 1);
        assert!(said.ends_with('\u{2026}'), "and it is visibly cut");

        let short = outcome_of_status(&TurnStatus::Failed {
            reason: "the model refused".to_owned(),
        });
        assert_eq!(short.error.as_deref(), Some("the model refused"));
    }

    #[tokio::test]
    async fn a_run_that_worked_the_card_is_not_told_it_produced_nothing() {
        let (board, run) = board_with_in_progress_card().await;
        let before = chrono::Utc::now();
        assert!(
            !board.projects.run_left_a_mark(&run, before).await,
            "nothing has happened on the card yet"
        );

        board
            .projects
            .comment(
                &board.project.id,
                run.number,
                IssueActor::Agent(run.agent_id.clone()),
                "pushed 51640fb and moved it to review",
                &[],
            )
            .await
            .expect("the run says what it did");

        assert!(
            board.projects.run_left_a_mark(&run, before).await,
            "the executor may say a run failed; it may not say the card is untouched"
        );
        assert!(
            !board
                .projects
                .run_left_a_mark(&run, chrono::Utc::now())
                .await,
            "and the window is the brief, not the whole card"
        );
    }
}
