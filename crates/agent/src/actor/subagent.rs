//! Agent-side wait routine the [`crate::runtime::subagent_spawner`] runs
//! after spawning a child subagent actor. All protocol value types live
//! in `baybo_model::spawn_protocol`; the `spawn_subagent` tool
//! (`baybo_subagent::tool`) hands its request to the spawner through the
//! `baybo_subagent::SubagentSpawner` capability.

use std::sync::Arc;

use baybo_channels::{AgentEvent, AgentOutput};
use baybo_model::{ContentBlock, SessionId, SubagentExitStatus, SubagentResult};
use baybo_turn::{TurnLifecycle, TurnLifecycleEvent, TurnStatusKind};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::actor::AgentMessage;
use crate::actor::mailbox::MailboxSender;

/// Wait for a freshly-spawned subagent to terminate. The caller (router)
/// owns the synchronous prelude (build child session, spawn actor, send
/// initial message); this routine then watches output_rx for the final
/// message + the lifecycle bus for the turn's terminal event (ignoring
/// the child's own start edge), with a store fallback on broadcast lag.
///
/// `actor_token` is the child actor's own
/// `VolatileResources::actor_token`, observed here so an external
/// cancel of the child surfaces as `Cancelled`.
pub async fn await_subagent_terminal(
    child_session_id: SessionId,
    mut output_rx: mpsc::Receiver<AgentOutput>,
    mut terminal_rx: broadcast::Receiver<TurnLifecycleEvent>,
    mailbox: MailboxSender<AgentMessage>,
    actor_token: CancellationToken,
    turn_lifecycle: Arc<TurnLifecycle>,
) -> SubagentResult {
    let mut captured: Option<Vec<ContentBlock>> = None;
    let wait_result = async {
        loop {
            tokio::select! {
                _ = actor_token.cancelled() => {
                    return Err(SubagentExitStatus::Cancelled);
                }
                msg = output_rx.recv() => {
                    match msg {
                        Some(AgentOutput {
                            event: AgentEvent::Message(m),
                            ..
                        }) => {
                            captured = Some(m.content);
                        }
                        Some(_) => continue,
                        None => return Err(SubagentExitStatus::Failed {
                            reason: "child output channel closed before terminal event".into(),
                        }),
                    }
                }
                event = terminal_rx.recv() => {
                    match event {
                        Ok(ev) if ev.session_id == child_session_id => {
                            // The bus now also carries the child's own start
                            // edge — ignore it and keep waiting for a terminal
                            // one.
                            let Some(kind) = ev.phase.terminal_status() else {
                                continue;
                            };
                            if matches!(kind, TurnStatusKind::Completed) && captured.is_none() {
                                // `TurnLifecycle::complete` publishes the terminal
                                // event inside `with_turn` BEFORE
                                // `handle_subagent_spawned` dispatches the final
                                // `AgentEvent::Message`. Drain `output_rx` so the
                                // queued message isn't lost when the terminal
                                // event wins the select with `captured == None`.
                                captured = drain_for_final_message(&mut output_rx).await;
                            }
                            return terminal_event_to_status(kind, captured.take());
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(
                                session_id = %child_session_id,
                                skipped = n,
                                "subagent waiter lagged on lifecycle-event bus, reconciling via store"
                            );
                            if let Some(kind) = check_child_terminal_via_store(
                                &turn_lifecycle,
                                &child_session_id,
                            ).await {
                                if matches!(kind, TurnStatusKind::Completed) && captured.is_none() {
                                    captured = drain_for_final_message(&mut output_rx).await;
                                }
                                return terminal_event_to_status(kind, captured.take());
                            }
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(SubagentExitStatus::Failed {
                                reason: "turn lifecycle-event bus closed".into(),
                            });
                        }
                    }
                }
            }
        }
    }
    .await;

    let _ = mailbox.send(AgentMessage::ActorStop).await;

    match wait_result {
        Ok(content) => SubagentResult {
            child_session_id,
            final_content: content,
            status: SubagentExitStatus::Completed,
        },
        Err(status) => SubagentResult {
            child_session_id,
            final_content: None,
            status,
        },
    }
}

fn terminal_event_to_status(
    kind: TurnStatusKind,
    captured: Option<Vec<ContentBlock>>,
) -> Result<Option<Vec<ContentBlock>>, SubagentExitStatus> {
    match kind {
        TurnStatusKind::Completed => Ok(captured),
        TurnStatusKind::Failed => Err(SubagentExitStatus::Failed {
            reason: "child turn failed".into(),
        }),
        TurnStatusKind::Cancelled => Err(SubagentExitStatus::Cancelled),
        other => Err(SubagentExitStatus::Failed {
            reason: format!("unexpected non-terminal terminal-event kind: {other:?}"),
        }),
    }
}

/// Single-query store reconcile for the broadcast-lag path. Lists every
/// turn on the child session, then picks the first terminal one — saves
/// the 3× round-trip of the per-status query the previous shape did.
async fn check_child_terminal_via_store(
    turn_lifecycle: &TurnLifecycle,
    child_session_id: &SessionId,
) -> Option<TurnStatusKind> {
    let turns = match turn_lifecycle.list_by_session(child_session_id, None).await {
        Ok(j) => j,
        Err(e) => {
            warn!(
                session_id = %child_session_id,
                error = %e,
                "subagent reconcile via store failed; will keep waiting"
            );
            return None;
        }
    };
    turns.into_iter().map(|j| j.status.kind()).find(|kind| {
        matches!(
            kind,
            TurnStatusKind::Completed | TurnStatusKind::Failed | TurnStatusKind::Cancelled
        )
    })
}

/// Drain `output_rx` until the child actor's final `AgentEvent::Message`
/// arrives (or the channel closes). Other `AgentOutput` variants
/// (`AnswerDelta`, `Notice`) are skipped — only `Message` carries the
/// subagent's final content. Bounded by the caller's outer timeout.
async fn drain_for_final_message(
    output_rx: &mut mpsc::Receiver<AgentOutput>,
) -> Option<Vec<ContentBlock>> {
    while let Some(out) = output_rx.recv().await {
        if let AgentOutput {
            event: AgentEvent::Message(m),
            ..
        } = out
        {
            return Some(m.content);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::mailbox::{self, MailboxReceiver};
    use baybo_channels::OutgoingMessage;
    use baybo_model::{ChannelType, MessageMetadata, TriggerKind};
    use baybo_turn::test_support::MemoryTurnStore;
    use baybo_turn::{TurnInput, TurnOutput};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    const CHILD_SESSION: &str = "child";

    struct Harness {
        turn_lifecycle: Arc<TurnLifecycle>,
        terminal_rx: tokio::sync::broadcast::Receiver<TurnLifecycleEvent>,
        output_tx: mpsc::Sender<AgentOutput>,
        output_rx: mpsc::Receiver<AgentOutput>,
        mailbox_tx: MailboxSender<AgentMessage>,
        _mailbox_rx: MailboxReceiver<AgentMessage>,
        actor_token: CancellationToken,
    }

    impl Harness {
        fn new() -> Self {
            let turn_lifecycle = Arc::new(TurnLifecycle::new(Arc::new(MemoryTurnStore::new())));
            let terminal_rx = turn_lifecycle.subscribe_lifecycle_events();
            let (output_tx, output_rx) = mpsc::channel(1);
            let (mailbox_tx, _mailbox_rx) = mailbox::channel(1);
            Self {
                turn_lifecycle,
                terminal_rx,
                output_tx,
                output_rx,
                mailbox_tx,
                _mailbox_rx,
                actor_token: CancellationToken::new(),
            }
        }

        /// Drop the only sender so the waiter's `output_rx.recv()`
        /// resolves to `None` — mimics a child actor that exited
        /// before emitting a final Message.
        fn close_output(&mut self) {
            let (closed, _drop_rx) = mpsc::channel(1);
            self.output_tx = closed;
        }

        fn spawn_waiter(self) -> Waiter {
            Waiter {
                actor_token: self.actor_token.clone(),
                handle: tokio::spawn(await_subagent_terminal(
                    SessionId::from(CHILD_SESSION),
                    self.output_rx,
                    self.terminal_rx,
                    self.mailbox_tx,
                    self.actor_token,
                    self.turn_lifecycle,
                )),
                output_tx: self.output_tx,
                _mailbox_rx: self._mailbox_rx,
            }
        }
    }

    /// Create + start an in-progress `Spawned` turn on the child session.
    /// Mirrors what `with_turn` does for a real subagent so tests can
    /// drive `TurnLifecycle::complete` / observe `TurnLifecycle::cancel`
    /// against a real row.
    async fn start_in_progress_child_turn(lc: &TurnLifecycle) -> baybo_model::TurnId {
        let turn = lc
            .start_turn(
                SessionId::from(CHILD_SESSION),
                TriggerKind::User,
                TurnInput::Spawned {
                    initial_prompt: vec![ContentBlock::Text("task".into())],
                },
                None,
            )
            .await
            .unwrap();
        lc.start(&turn.id).await.unwrap();
        turn.id
    }

    fn outgoing(text: &str) -> AgentOutput {
        OutgoingMessage {
            session_id: SessionId::from(CHILD_SESSION),
            user_id: String::new(),
            channel: ChannelType::from("subagent"),
            content: vec![ContentBlock::Text(text.into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
            ordinal: None,
        }
        .into()
    }

    /// Spawned waiter plus the peer endpoints that must outlive it —
    /// dropping `output_tx` here would close `output_rx` mid-test and
    /// surface `Failed` regardless of intent. Tests that need the
    /// `TurnLifecycle` after spawn clone it from the `Harness` before
    /// calling `spawn_waiter`.
    struct Waiter {
        actor_token: CancellationToken,
        handle: JoinHandle<SubagentResult>,
        output_tx: mpsc::Sender<AgentOutput>,
        _mailbox_rx: MailboxReceiver<AgentMessage>,
    }

    impl Waiter {
        async fn finish(self) -> SubagentResult {
            self.handle.await.unwrap()
        }
    }

    #[tokio::test]
    async fn external_cancel_surfaces_as_cancelled() {
        let w = Harness::new().spawn_waiter();
        w.actor_token.cancel();
        let result = w.finish().await;
        assert!(matches!(result.status, SubagentExitStatus::Cancelled));
    }

    #[tokio::test]
    async fn output_channel_close_surfaces_as_failed() {
        let mut h = Harness::new();
        h.close_output();
        let result = h.spawn_waiter().finish().await;
        assert!(matches!(result.status, SubagentExitStatus::Failed { .. }));
    }

    /// `TurnLifecycle::complete` publishes the terminal event inside
    /// `with_turn` BEFORE `handle_subagent_spawned` dispatches the final
    /// `AgentEvent::Message`. The waiter must keep draining `output_rx`
    /// after observing `Completed` with `captured == None`, otherwise
    /// the queued final Message is lost and the parent sees an empty
    /// "subagent completed without producing a final message" answer.
    #[tokio::test]
    async fn completed_event_drains_late_final_message() {
        let h = Harness::new();
        let lc = Arc::clone(&h.turn_lifecycle);
        let turn_id = start_in_progress_child_turn(&lc).await;

        let w = h.spawn_waiter();

        // Publish terminal Completed BEFORE the final Message lands on
        // output_rx — the precise race the production path produces.
        lc.complete(
            &turn_id,
            TurnOutput::Message {
                content: vec![ContentBlock::Text("done".into())],
                ordinal: None,
            },
        )
        .await
        .unwrap();
        // Hand the runtime a tick so the waiter pulls the terminal
        // event and enters drain_for_final_message before we send.
        tokio::time::sleep(Duration::from_millis(20)).await;
        w.output_tx.send(outgoing("hello")).await.unwrap();

        let result = w.finish().await;
        assert!(
            matches!(result.status, SubagentExitStatus::Completed),
            "expected Completed, got {:?}",
            result.status
        );
        let content = result
            .final_content
            .expect("drain must surface the late final Message");
        assert!(matches!(&content[..], [ContentBlock::Text(t)] if t == "hello"));
    }
}
