//! Actor task lifetime and panic recovery.

use std::sync::Arc;

use baybo_channels::{AgentEvent, AgentOutput, NoticeLevel};
use baybo_trace::TraceStore;
use chrono::Utc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::warn;

use crate::actor::mailbox::MailboxReceiver;
use crate::actor::{AgentActor, AgentMessage};
use crate::recovery::recover_panicked_actor_session;

/// Tracks every actor spawned by the router, including unregistered one-shots,
/// until its final save or panic recovery completes.
#[derive(Clone)]
pub struct ActorTasks {
    parent: CancellationToken,
    tasks: TaskTracker,
    trace_store: Arc<dyn TraceStore>,
}

impl ActorTasks {
    pub fn new(parent: CancellationToken, trace_store: Arc<dyn TraceStore>) -> Self {
        Self {
            parent,
            tasks: TaskTracker::new(),
            trace_store,
        }
    }

    pub fn parent_token(&self) -> CancellationToken {
        self.parent.clone()
    }

    pub fn spawn(&self, actor: AgentActor, mailbox: MailboxReceiver<AgentMessage>) {
        // Reserve before checking admission: a concurrent shutdown must either
        // see this task when it waits or prevent the actor from starting.
        let task =
            self.tasks
                .track_future(run_actor(actor, mailbox, Arc::clone(&self.trace_store)));
        if !self.parent.is_cancelled() {
            tokio::spawn(task);
        }
    }

    pub async fn shutdown(&self) {
        self.parent.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

async fn run_actor(
    actor: AgentActor,
    mailbox: MailboxReceiver<AgentMessage>,
    trace_store: Arc<dyn TraceStore>,
) {
    let session_id = actor.durable.session.id.clone();
    let user_id = actor.durable.session.user.id.clone();
    let channel = actor.durable.session.channel.clone();
    let response_tx = actor.volatile.response_tx.clone();
    let turn_lifecycle = Arc::clone(&actor.volatile.turn_lifecycle);
    let task = tokio::spawn(actor.run(mailbox));
    let Err(join_err) = task.await else { return };
    if !join_err.is_panic() {
        return;
    }
    warn!(
        %session_id,
        error = %join_err,
        "agent actor panicked; recovering in-flight turns"
    );
    let summary =
        recover_panicked_actor_session(trace_store, turn_lifecycle, &session_id, Utc::now()).await;
    if summary.turns_cancelled > 0 {
        let notice = AgentOutput {
            session_id,
            user_id,
            channel,
            event: AgentEvent::Notice {
                level: NoticeLevel::Error,
                text: "The assistant crashed mid-turn; the turn was cancelled.".to_string(),
                mid_turn: false,
                durable_id: None,
            },
        };
        let _ = response_tx.send(notice).await;
    }
}
