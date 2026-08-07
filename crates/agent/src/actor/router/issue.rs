//! Executing a board issue's run.
//!
//! The ledger row already exists — `baybo-project` wrote it before anything
//! was told about it. This module is the consumer: it claims the row, mints
//! or reuses the session its agent works the card in, runs the brief as one
//! turn, and settles the row with what happened.
//!
//! The shape is cron's one-shot fire (`cron.rs`), with two differences that
//! matter. An issue keeps **one session per agent that has run it**, so a
//! follow-up sees what that same agent did last time — and an agent whose
//! runs have not opened one sees none of the transcript, which is why the
//! brief it is given is the whole conversation rather than a delta
//! (`baybo::runtime::issue_brief`, bounded by [`session_run_before`] so the
//! two answers cannot drift apart). That is safe only because at most one
//! run per issue is ever in flight (a partial unique index enforces it),
//! which is what lets the waiter treat the terminal turn at or after its
//! own enqueue as unambiguously its own. And nothing is dispatched to a
//! channel: an issue's audience is its card.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use baybo_model::{
    AgentBinding, AgentProfileId, ChannelType, Session, SessionId, TriggerSource, TurnId, User,
};
use baybo_project::{ProjectEvents, ProjectManager, worktree};
use baybo_store::project::{
    IssueActor, IssueEventBody, IssueRunRow, NewIssueEvent, ProjectStore, RunStatus,
};
use baybo_turn::{
    CancelReason, TurnInputKind, TurnLifecycle, TurnLifecycleEvent, TurnStatus, TurnStatusKind,
};
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
            enqueued: event.run.clone(),
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
    /// hands the card has passed through since. Which run that is comes
    /// from [`session_run_to_continue`] — the one rule for the question,
    /// shared with the brief window.
    async fn issue_session(
        &self,
        store: &Arc<dyn ProjectStore>,
        event: &IssueRunEvent,
    ) -> anyhow::Result<Session> {
        let issue = store
            .get_issue(&event.run.project_id, event.run.number)
            .await?
            .ok_or_else(|| anyhow::anyhow!("issue #{} is gone", event.run.number))?;

        // Before either branch, not only the minting one: a session opened
        // while the agent was on baybo must not be handed a run the agent
        // can no longer host.
        let binding = self.binding_for(&event.run.agent_id).await?;

        let runs = store.list_runs(&issue.id).await?;
        // The binding check is the write-once guard, not the selection: the
        // row's `session_id` is what says which session, and a session whose
        // binding disagrees with the row is a broken pairing this run must
        // not be handed into whatever the ledger says.
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

    /// The binding this run's session is opened with, read from the agent's
    /// profile rather than assumed.
    ///
    /// Every other binding in the tree is built from the row — the lead's, the
    /// chat leg's, cron's. This one used to name `Baybo` outright, which was
    /// true only because [`baybo_project::can_host_a_session`] refuses anything
    /// else at assign time. That answer expires: a profile's framework is
    /// editable, and a row the boot sweep re-drives was recorded under whatever
    /// the agent was then. So the fact is re-established here, at the last
    /// moment before it is written into a session write-once.
    ///
    /// Refused rather than recorded, because a top-level session bound to an
    /// external backend would still be executed by the internal loop — the card
    /// would name an agent that never worked it. The run settles `Failed` with
    /// this reason on its card, which is the visible half of the same refusal
    /// the operator would have got from the assign form.
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

/// Whether this run ever got as far as being picked up — the router's name
/// for [`IssueRunRow::was_claimed`], which is where the rule lives.
///
/// It is spelled again here only because this crate's callers read better
/// for it; `baybo-project` asks the same question of the same row to pick
/// the sentence a called-off run settles with, and it cannot depend on
/// `baybo-agent` without a cycle. One rule, one home, two names.
///
/// Note what it does **not** say. The executor claims the row before the
/// actor is spawned, so a claimed run is one the card has announced as
/// started — not necessarily one that opened a turn. A run whose trigger
/// never reached its actor leaves a claimed row and an empty transcript,
/// and callers that care about the transcript rather than the announcement
/// are reading a slightly stronger fact than this answers.
pub fn ever_ran(run: &IssueRunRow) -> bool {
    run.was_claimed()
}

/// The run whose session this one is handed, if any — **the** rule for
/// "has this agent worked this card before".
///
/// [`Router::issue_session`] hands the run into this row's session and
/// mints a fresh one when there is none. `baybo`'s `brief_window` asks the
/// same question through [`session_run_before`] to decide how much of the
/// card's conversation this run has already read, and the two must agree:
/// a brief bounded by a run whose session this one is *not* given trims the
/// conversation as "already read" against a transcript that does not
/// contain it. This function is the authority; the window follows it.
///
/// This run's own row is a candidate, which is what lets a run
/// re-dispatched by the boot sweep resume the session it already claimed.
fn session_run_to_continue<'a>(
    run: &IssueRunRow,
    runs: &'a [IssueRunRow],
) -> Option<&'a IssueRunRow> {
    newest_run_that_ran(&run.agent_id, runs.iter())
}

/// `session_run_to_continue`'s rule over the runs *before* this one — the
/// run whose turn is already in the transcript this one opens, and so the
/// point the card's conversation is a delta from.
///
/// This is what `baybo`'s `brief_window` reads. It is exported for that one
/// caller: the window is not allowed a rule of its own.
///
/// Excludes this run's own row on purpose — bounding a brief by the clock
/// of the run being briefed would filter out the very comment that started
/// it.
pub fn session_run_before<'a>(
    run: &IssueRunRow,
    runs: &'a [IssueRunRow],
) -> Option<&'a IssueRunRow> {
    newest_run_that_ran(
        &run.agent_id,
        runs.iter().filter(|candidate| candidate.id != run.id),
    )
}

/// Newest by attempt rather than by clock: attempts are handed out in
/// order by the store, under the same transaction that writes the row.
fn newest_run_that_ran<'a>(
    agent: &AgentProfileId,
    runs: impl Iterator<Item = &'a IssueRunRow>,
) -> Option<&'a IssueRunRow> {
    runs.filter(|candidate| &candidate.agent_id == agent && ever_ran(candidate))
        .max_by_key(|candidate| candidate.attempt)
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
/// A run **a human stopped** starts nothing, whatever was said. The card is
/// still InProgress and still assigned, so the board would happily answer
/// `Wake` — and a fresh run seconds after somebody pressed Cancel reads as
/// the Stop button not working.
///
/// Every other ending follows up, and that includes a run that merely
/// *settled* `Cancelled`: an actor that dies takes its turn to
/// `Cancelled { SystemCrash }`, and the ledger row cannot tell that apart
/// from a Stop. Keying on the row's status would drop the comment in
/// exactly the case this mechanism exists for — see [`RunOutcome`].
async fn follow_up_on_comments(
    projects: &Arc<ProjectManager>,
    store: &Arc<dyn ProjectStore>,
    run: &IssueRunRow,
    outcome: &RunOutcome,
) {
    if outcome.stopped_by_a_human {
        debug!(run = %run.id, "somebody stopped this run; not starting a follow-up on it");
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
    /// The run as it was **enqueued** — the row the dispatcher handed over,
    /// cloned before `claim_run` stamped it. Its `status` therefore reads
    /// `Queued` and its `started_at` is empty for the whole life of the
    /// turn, however long that is. Only what never changes is read from it —
    /// the ids, the number, the attempt, `created_at` — and what the settle
    /// needs to know about how the run *ended* comes from the turn.
    enqueued: IssueRunRow,
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
        let outcome = self.await_run(actor_token).await;
        let run_id = &self.enqueued.id;
        let status = outcome.status;
        info!(%run_id, ?status, "issue run settled");
        let store = &self.board.store;
        settle(
            store,
            &self.board.events,
            &self.enqueued,
            status,
            outcome.error.as_deref(),
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
        surface_branch(store, &self.board.events, &self.checkout, &self.enqueued).await;
        follow_up_on_comments(&self.board.manager, store, &self.enqueued, &outcome).await;
    }

    async fn await_run(&mut self, actor_token: CancellationToken) -> RunOutcome {
        loop {
            tokio::select! {
                event = self.terminal_rx.recv() => match event {
                    Ok(ev) if self.is_our_run(&ev) => {
                        let Some(kind) = ev.phase.terminal_status() else {
                            continue;
                        };
                        return self.outcome_of_edge(&ev.turn_id, kind).await;
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            run_id = %self.enqueued.id,
                            skipped = n,
                            "issue waiter lagged on the lifecycle bus; reconciling via store"
                        );
                        if let Some(outcome) = self.reconcile().await {
                            return outcome;
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return RunOutcome::failed("lifecycle bus closed before the run finished");
                    }
                },
                _ = actor_token.cancelled() => {
                    // The actor is gone: it either finished and we have not
                    // drained its event, or it died before opening a turn.
                    // The store says which.
                    if let Some(outcome) = self.reconcile().await {
                        return outcome;
                    }
                    return RunOutcome::failed("the run stopped before producing anything");
                }
            }
        }
    }

    fn is_our_run(&self, ev: &TurnLifecycleEvent) -> bool {
        ev.session_id == self.session_id && ev.kind == TurnInputKind::IssueRun
    }

    /// The outcome of a terminal edge seen on the lifecycle bus.
    ///
    /// The edge says a turn was cancelled but not why, and why is the whole
    /// question — so a cancel, and only a cancel, goes back to the turn for
    /// its [`CancelReason`]. The row is written before the edge is
    /// published, so the reason is already durable by the time this reads
    /// it.
    ///
    /// A reason the store cannot produce answers "not a human": one extra
    /// follow-up run is visible on the card and stoppable, while a comment
    /// nothing ever answers is the loss the follow-up exists to prevent.
    async fn outcome_of_edge(&self, turn: &TurnId, kind: TurnStatusKind) -> RunOutcome {
        let outcome = RunOutcome::of(kind);
        if kind != TurnStatusKind::Cancelled {
            return outcome;
        }
        let stopped_by_a_human = match self.lifecycle.get(turn).await {
            Ok(Some(turn)) => stopped_by_a_human(&turn.status),
            Ok(None) => false,
            Err(e) => {
                warn!(run_id = %self.enqueued.id, error = %e, "could not read why the run's turn was cancelled");
                false
            }
        };
        RunOutcome {
            stopped_by_a_human,
            ..outcome
        }
    }

    /// The run's outcome from the store, or `None` if this run has no
    /// finished turn.
    ///
    /// Bounded at both ends. **Newest** terminal issue turn, not the first:
    /// this session hosts every run its agent has worked on this issue, so
    /// the first one is that agent's first run forever. And **at or after
    /// this run's own enqueue**, because everything older belongs to a
    /// previous run of the same card — settling on one of those would hand
    /// this run its predecessor's outcome, `stopped_by_a_human` and all,
    /// and a run wrongly reading as stopped-by-a-human drops the comment
    /// waiting on the card. A run whose actor never opened a turn has
    /// nothing in the window, and the caller settles it as having failed —
    /// which is what happened.
    ///
    /// The turn is created inside the actor the claim spawns, so the row
    /// this run is executing can only be older than its own turn; `>=`
    /// rather than `>` because the two clocks are the same wall clock and a
    /// turn opened in the same microsecond is still this run's.
    ///
    /// That there is exactly one candidate in that window is the dedupe
    /// guard's doing: an issue holds at most one unfinished run at a time.
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
            .map(|t| RunOutcome {
                stopped_by_a_human: stopped_by_a_human(&t.status),
                ..RunOutcome::of(t.status.kind())
            })
    }
}

/// How a run ended: what the ledger row records, plus the one thing the row
/// cannot carry.
///
/// `RunStatus::Cancelled` is written both when somebody presses Stop and
/// when the run dies — crash recovery rolls an orphaned turn to
/// `Cancelled { SystemCrash }`, and a cancel propagated down the token tree
/// arrives as `ParentCancelled`. The turn's [`CancelReason`] is the only
/// place the two are distinguishable, so it is read once here rather than
/// guessed at from the status by whoever needs the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunOutcome {
    status: RunStatus,
    error: Option<String>,
    /// A person asked for this to stop, as opposed to it stopping on its
    /// own. Never true for a status other than `Cancelled`.
    stopped_by_a_human: bool,
}

impl RunOutcome {
    /// What a terminal turn status settles the row as. Says nothing about
    /// who stopped it — that is [`stopped_by_a_human`], which needs the
    /// reason the kind has already dropped.
    fn of(kind: TurnStatusKind) -> Self {
        match kind {
            TurnStatusKind::Completed => Self {
                status: RunStatus::Done,
                error: None,
                stopped_by_a_human: false,
            },
            TurnStatusKind::Cancelled => Self {
                status: RunStatus::Cancelled,
                error: None,
                stopped_by_a_human: false,
            },
            TurnStatusKind::Failed => Self::failed("the run failed; its transcript has the detail"),
            other => Self::failed(format!("the run ended as {other:?}")),
        }
    }

    fn failed(error: impl Into<String>) -> Self {
        Self {
            status: RunStatus::Failed,
            error: Some(error.into()),
            stopped_by_a_human: false,
        }
    }
}

/// Whether a cancelled turn was cancelled because a person asked for it.
///
/// The operator's Cancel button on the card reaches the turn as
/// [`CancelReason::OperatorCancel`] (so does the CLI), and `/stop` inside
/// the session as [`CancelReason::UserStopped`]. Every other reason is a
/// run that stopped without anybody asking this run to stop: an actor that
/// panicked, a parent that went away or was deleted, a subagent that timed
/// out, a process killed and swept at boot. `UserPreempt` sits on that side
/// too — it belongs to a chat turn superseded by the next message, which a
/// one-shot issue actor has no path to.
///
/// Matched exhaustively rather than with `matches!`: this is a
/// classification over another crate's enum, and a reason added there must
/// break this build rather than default to "not a human" and quietly
/// falsify the paragraph above.
///
/// `ParentCancelled` reads as not-a-human, and that is the deliberate side
/// of a race: `TurnLifecycle::cancel` trips the token before it writes the
/// row, and the run's own body settles the row `ParentCancelled` when it
/// unwinds — whichever gets there first is the reason that sticks. The
/// window is open on **every** press, not only on one that arrives as the
/// run was finishing; how often the operator loses it is a function of what
/// the body was awaiting, and it is often enough to see. Erring the other
/// way would make every shutdown look like a Stop and drop the comment it
/// was carrying; erring this way costs one follow-up run, visible on the
/// card and stoppable again. Closing it for real needs the intended reason
/// recorded before the token is tripped, which is `baybo-turn`'s to do.
fn stopped_by_a_human(status: &TurnStatus) -> bool {
    let TurnStatus::Cancelled { reason, .. } = status else {
        return false;
    };
    match reason {
        CancelReason::OperatorCancel | CancelReason::UserStopped => true,
        CancelReason::UserPreempt
        | CancelReason::SystemCrash
        | CancelReason::SubagentTimeout
        | CancelReason::ParentCancelled
        | CancelReason::ParentDeleted => false,
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

        follow_up_on_comments(
            &board.projects,
            &board.store,
            &first,
            &RunOutcome::of(TurnStatusKind::Completed),
        )
        .await;

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

        follow_up_on_comments(
            &board.projects,
            &board.store,
            &first,
            &RunOutcome::of(TurnStatusKind::Completed),
        )
        .await;

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

    /// How the waiter reads a run whose turn ended in `reason` — the one
    /// input that separates a Stop somebody pressed from a run that died,
    /// since both settle the ledger row `Cancelled`.
    ///
    /// Asserts the bus edge and the store reconcile agree: production takes
    /// the edge, the lagged and actor-died paths take the reconcile, and a
    /// difference between them would be a comment lost on whichever path
    /// the timing picked.
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
            checkout: PathBuf::from("/tmp/does-not-matter"),
            session_id: session.clone(),
            terminal_rx: lifecycle.subscribe_lifecycle_events(),
            lifecycle: Arc::clone(&lifecycle),
            board: BoardWiring {
                store: Arc::clone(&board.store),
                events: Arc::new(baybo_project::NoopProjectEvents),
                manager: Arc::clone(&board.projects),
            },
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

    /// Cancel is the operator saying stop. The card stays InProgress and
    /// stays assigned, so the board would happily answer "wake" — and a
    /// fresh run seconds after the Cancel button reads as the button not
    /// working.
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
        board
            .store
            .settle_run(&first.id, outcome.status, None)
            .await
            .expect("settle");

        follow_up_on_comments(&board.projects, &board.store, &first, &outcome).await;

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

    /// A run that *died* settles `Cancelled` too: crash recovery rolls the
    /// turn of an actor that panicked to `Cancelled { SystemCrash }`, and a
    /// cancel that propagated down the token tree arrives as
    /// `ParentCancelled`. Nobody pressed Stop in either case, so the comment
    /// waiting on the card still has to start something — that loss is the
    /// whole reason the follow-up exists.
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
            board
                .store
                .settle_run(&first.id, outcome.status, None)
                .await
                .expect("settle");

            follow_up_on_comments(&board.projects, &board.store, &first, &outcome).await;

            let dispatched = board.dispatched.lock().clone();
            assert_eq!(
                dispatched.len(),
                2,
                "a run that died on {reason:?} must not swallow the comment"
            );
            assert_eq!(dispatched[1].trigger, RunTrigger::Comment);
        }
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
            window_and_session_agree(&board, &harness, &back).await.id,
            first_session.id,
            "an agent handed the card back continues in the session it already worked it in"
        );
    }

    /// The brief window and the session rule answer one question, and this
    /// is where they are held to it.
    ///
    /// `issue_session` is the authority: it decides which session — and so
    /// which transcript — this run is handed. `session_run_before` is what
    /// `baybo`'s `brief_window` reads to decide how much of the card's
    /// conversation the run has already seen. Either the run continues a
    /// previous run's session, and the window is that run; or it does not,
    /// and the window is the whole card. Anything else trims a conversation
    /// as read against a transcript that does not contain it.
    async fn window_and_session_agree(
        board: &Board,
        harness: &RouterHarness,
        run: &IssueRunRow,
    ) -> Session {
        let runs = board.store.list_runs(&run.issue_id).await.expect("runs");
        let session = harness
            .router
            .issue_session(&board.store, &run_event(run))
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

    /// The narrower shape of the same disagreement: a *same-agent* run that
    /// never claimed a session.
    ///
    /// The operator presses Cancel while the run is still queued, so the
    /// manager settles the row where it stands and no executor ever claims
    /// it. The card is still InProgress and still assigned, so the next
    /// comment wakes the same agent again — into a session with nothing in
    /// it, because there is no claimed session to continue. A window that
    /// merely matched on `agent_id` would call that a follow-up and trim
    /// the whole discussion that set the work up.
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

    /// A run settles on its own turn or on nothing.
    ///
    /// The session hosts every run its agent has worked on this card, so
    /// the newest terminal turn in it is the *previous* run's until this
    /// one opens its own. A run whose actor never got that far — the
    /// mailbox send failed, so `handle_issue_run` cancels the token — has
    /// to read as having failed. Inheriting instead means inheriting
    /// `stopped_by_a_human` from an operator's Cancel that was aimed at a
    /// run which has already ended, and the comment waiting on the card
    /// then starts nothing.
    #[tokio::test]
    async fn a_run_whose_actor_never_opened_a_turn_does_not_inherit_the_last_ones_outcome() {
        let (board, first) = mid_run().await;
        let session = SessionId::from("sess-issue-1");
        let lifecycle = Arc::new(TurnLifecycle::new(Arc::new(
            baybo_turn::test_support::MemoryTurnStore::new(),
        )));

        // Run #1's turn, stopped by the operator.
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

        // A comment then asks the same agent again, in the same session.
        board
            .projects
            .comment(
                &board.project.id,
                1,
                IssueActor::User,
                "start with the CSV path",
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
        // And somebody says something else while run #2 is live, which is
        // what the follow-up exists to carry.
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

        let mut waiter = IssueRunWaiter {
            enqueued: second.clone(),
            checkout: PathBuf::from("/tmp/does-not-matter"),
            session_id: session.clone(),
            terminal_rx: lifecycle.subscribe_lifecycle_events(),
            lifecycle: Arc::clone(&lifecycle),
            board: BoardWiring {
                store: Arc::clone(&board.store),
                events: Arc::new(baybo_project::NoopProjectEvents),
                manager: Arc::clone(&board.projects),
            },
        };
        assert_eq!(
            waiter.reconcile().await,
            None,
            "run #1's ending is not run #2's outcome"
        );

        let actor_token = CancellationToken::new();
        actor_token.cancel();
        let outcome = waiter.await_run(actor_token).await;
        assert_eq!(
            outcome.status,
            RunStatus::Failed,
            "a run that produced nothing failed; it was not cancelled"
        );
        assert!(
            !outcome.stopped_by_a_human,
            "and nobody stopped it — the Cancel it would have inherited was aimed at run #1"
        );

        board
            .store
            .settle_run(&second.id, outcome.status, outcome.error.as_deref())
            .await
            .expect("settle");
        follow_up_on_comments(&board.projects, &board.store, &second, &outcome).await;
        let dispatched = board.dispatched.lock().clone();
        assert_eq!(
            dispatched.len(),
            3,
            "so the comment left during run #2 still starts something"
        );
        assert_eq!(dispatched[2].trigger, RunTrigger::Comment);
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

    /// The binding is read, not assumed — and this is the path that needs it
    /// to be. `enqueue` refuses a card whose agent has moved off baybo, but
    /// the sweeps hand out rows recorded *earlier*, so a row written while
    /// the agent was baybo arrives here after the flip having passed no gate
    /// at all. Binding it `Baybo` anyway would run a codex agent's persona
    /// on the internal loop and sign its name to the commits.
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
            .issue_session(&board.store, &run_event(&run))
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
}
