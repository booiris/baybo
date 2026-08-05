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

use std::sync::Arc;

use baybo_model::{
    AgentBinding, AgentProfileId, ChannelType, Session, SessionId, TriggerSource, User,
};
use baybo_project::ProjectEvents;
use baybo_store::project::{IssueRunRow, ProjectStore, RunStatus};
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
    /// Who the run belongs to, for session ownership.
    pub user_id: String,
    pub channel: ChannelType,
}

impl Router {
    pub(super) async fn handle_issue_run(&mut self, event: IssueRunEvent) {
        let run_id = event.run.id.clone();
        let Some(store) = self.project_store.clone() else {
            warn!(%run_id, "issue run arrived with no project store; cannot execute");
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

        // Subscribed before the trigger is sent, and this is load-bearing:
        // the terminal event is published from inside the run's own turn, so
        // a subscription opened afterwards can miss a fast failure entirely.
        let waiter = IssueRunWaiter {
            run: event.run.clone(),
            session_id: session.id.clone(),
            lifecycle: Arc::clone(&self.turn_lifecycle),
            terminal_rx: self.turn_lifecycle.subscribe_lifecycle_events(),
            store: Arc::clone(&store),
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

async fn settle(
    store: &Arc<dyn ProjectStore>,
    events: Option<&Arc<dyn ProjectEvents>>,
    run: &IssueRunRow,
    status: RunStatus,
    error: Option<&str>,
) {
    match store.settle_run(&run.id, status, error).await {
        // Announce only a settle that actually landed: a replay of an
        // already-settled run changed nothing and has nothing to say.
        Ok(true) => {
            if let Some(events) = events {
                events.run_changed(&run.project_id, run.number);
            }
        }
        Ok(false) => {}
        Err(e) => {
            let run_id = &run.id;
            warn!(%run_id, error = %e, "could not settle run; the boot sweep will retry it");
        }
    }
}

/// Watches one run's turn and settles its ledger row.
struct IssueRunWaiter {
    run: IssueRunRow,
    session_id: SessionId,
    lifecycle: Arc<TurnLifecycle>,
    terminal_rx: broadcast::Receiver<TurnLifecycleEvent>,
    store: Arc<dyn ProjectStore>,
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
