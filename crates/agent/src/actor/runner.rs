//! Actor task runner and panic recovery.
//!
//! The supervisor owns the actor registry and routing policy. This module
//! owns the tokio task boundary around `AgentActor::run`: if the task exits
//! by panic, it delegates durable cleanup to `crate::recovery` and emits the
//! user-facing crash notice.

use std::sync::Arc;

use baybo_channels::{AgentEvent, AgentOutput, NoticeLevel};
use baybo_model::{ChannelType, SessionId};
use baybo_trace::TraceStore;
use baybo_turn::TurnLifecycle;
use chrono::Utc;
use tokio::sync::mpsc;
use tracing::warn;

use crate::actor::mailbox::MailboxReceiver;
use crate::actor::{AgentActor, AgentMessage};
use crate::recovery::recover_panicked_actor_session;

/// Dependencies needed after an actor task panics.
///
/// Captured before the actor is moved into its task so the panic observer can
/// recover durable turn/trace state even though the actor itself has unwound.
pub struct ActorRunnerContext {
    pub session_id: SessionId,
    pub user_id: String,
    pub channel: ChannelType,
    pub response_tx: mpsc::Sender<AgentOutput>,
    pub turn_lifecycle: Arc<TurnLifecycle>,
    pub trace_store: Arc<dyn TraceStore>,
}

/// Spawn an actor's run loop and watch its task boundary for panic.
///
/// Normal exits need no durable cleanup: `ActorStop` is shutdown, mailbox
/// close means there is no owner left to route to, and `ActorRegistryGuard`
/// deregisters the actor during `AgentActor::run` teardown. A panic is
/// different: the in-flight `with_turn` future may have been dropped before it
/// could close trace rows or mark the turn terminal. The recovery layer owns
/// that repair, and its turn cancellation publishes the lifecycle event that
/// the TurnState projector converts into the web inactive edge.
pub fn spawn_actor(
    actor: AgentActor,
    mailbox: MailboxReceiver<AgentMessage>,
    ctx: ActorRunnerContext,
) {
    let task = tokio::spawn(async move {
        actor.run(mailbox).await;
    });
    tokio::spawn(async move {
        let Err(join_err) = task.await else { return };
        if !join_err.is_panic() {
            return;
        }
        warn!(
            session_id = %ctx.session_id,
            error = %join_err,
            "agent actor panicked; recovering in-flight turns"
        );
        let summary = recover_panicked_actor_session(
            Arc::clone(&ctx.trace_store),
            Arc::clone(&ctx.turn_lifecycle),
            &ctx.session_id,
            Utc::now(),
        )
        .await;
        if summary.turns_cancelled > 0 {
            let notice = AgentOutput {
                session_id: ctx.session_id.clone(),
                user_id: ctx.user_id.clone(),
                channel: ctx.channel.clone(),
                event: AgentEvent::Notice {
                    level: NoticeLevel::Error,
                    text: "The assistant crashed mid-turn; the turn was cancelled.".to_string(),
                    mid_turn: false,
                    durable_id: None,
                },
            };
            let _ = ctx.response_tx.send(notice).await;
        }
    });
}
