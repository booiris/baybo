//! Actor task lifetime and panic recovery.

use baybo_channels::{AgentEvent, AgentOutput, NoticeLevel};
use chrono::Utc;
use tracing::warn;

use crate::actor::mailbox::MailboxReceiver;
use crate::actor::{AgentActor, AgentMessage};
use crate::recovery::TurnRecovery;

pub(super) async fn run_actor(
    actor: AgentActor,
    mailbox: MailboxReceiver<AgentMessage>,
    recovery: TurnRecovery,
) {
    let session_id = actor.durable.session.id.clone();
    let user_id = actor.durable.session.user.id.clone();
    let channel = actor.durable.session.channel.clone();
    let response_tx = actor.volatile.response_tx.clone();
    let supervisor = actor.volatile.supervisor.clone();
    let result = tokio::spawn(actor.run(mailbox)).await;
    if let Some(supervisor) = supervisor {
        supervisor.remove_closed_mailbox(&session_id);
    }
    let Err(join_err) = result else { return };
    if !join_err.is_panic() {
        return;
    }
    warn!(
        %session_id,
        error = %join_err,
        "agent actor panicked; recovering in-flight turns"
    );
    let summary = recovery.on_actor_panic(&session_id, Utc::now()).await;
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
