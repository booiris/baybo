//! Eager channel installation at gateway boot.
//!
//! Walks the workspace [`ChannelsConfig`] and installs one
//! [`baybo_channels::Channel`] per enabled channel type into the shared
//! [`ChannelRegistry`]. Channels are pinned for the lifetime of the
//! gateway process — connections come and go (browser tabs, TUI
//! processes, telegram bot subprocesses) but the channel sits there
//! waiting for them.
//!
//! Each channel is born with its own [`ChannelAskGate`] over the channel's
//! [`AskQueue`], fetched from the shared [`AskQueues`] so a queue the boot
//! reload already filled is the one the channel attaches to. Frames are
//! dispatched by queue watchers installed here, not by the gate: an ask
//! entering the queue becomes
//! [`SessionEvent::ApprovalRequested`](baybo_channels::SessionEvent::ApprovalRequested),
//! and every ask leaving it — answered or abandoned — becomes
//! [`SessionEvent::AskResolved`](baybo_channels::SessionEvent::AskResolved),
//! plus [`SessionEvent::ApprovalResolved`](baybo_channels::SessionEvent::ApprovalResolved)
//! when a human decided an approval.

use std::sync::Arc;

use std::sync::Weak;

use baybo_channels::wire::AskOutcomeKind;
use baybo_channels::{AskSurface, Channel, ChannelKind, ChannelRegistry, Result as ChannelResult};
use baybo_config::ChannelsConfig;
use baybo_human_ask::{AskCore, AskQueue, AskQueueEvent, AskQueues, ChannelAskGate, PendingAsk};
use baybo_model::ChannelType;
use baybo_model::human_ask::AskResolution;
use baybo_tools::ApprovalGate;

use super::session_pulse::SessionPulse;

/// Built-in mapping from `channel_type` to [`ChannelKind`]. Compile-
/// time constant because it follows from the channel's protocol
/// shape (one process multiplexes many users = `Multiplexed`; one
/// process per view = `Subscribed`), not from operator preference.
fn kind_for(channel_type: &ChannelType) -> ChannelKind {
    match channel_type.as_str() {
        ChannelType::OWNER | ChannelType::TUI => ChannelKind::Subscribed,
        ChannelType::TELEGRAM | ChannelType::DISCORD | ChannelType::WEIXIN => {
            ChannelKind::Multiplexed
        }
        // Out-of-tree channels declared via `baybo.json` default to
        // `Multiplexed` — the common case is "one sidecar serves all
        // users". An operator who needs subscribed semantics for a
        // custom channel can wire it through this map.
        _ => ChannelKind::Multiplexed,
    }
}

/// Install all enabled channels from `config` into `registry`. Idempotent
/// behaviour is *not* guaranteed: a duplicate install on an already-
/// populated registry returns [`baybo_channels::ChannelError::DuplicateChannel`].
/// Call exactly once at gateway boot.
pub fn install_channels(
    registry: &Arc<ChannelRegistry>,
    config: &ChannelsConfig,
    core: &Arc<AskCore>,
    queues: &AskQueues,
) -> ChannelResult<()> {
    if config.cli.enabled {
        install_channel(registry, ChannelType::tui(), core, queues)?;
    }
    if config.telegram.as_ref().is_some_and(|c| c.enabled) {
        install_channel(registry, ChannelType::telegram(), core, queues)?;
    }
    if config.discord.as_ref().is_some_and(|c| c.enabled) {
        install_channel(registry, ChannelType::discord(), core, queues)?;
    }
    if config.weixin.as_ref().is_some_and(|c| c.enabled) {
        install_channel(registry, ChannelType::weixin(), core, queues)?;
    }
    // The single owner chat channel serves both the web dashboard and the
    // mobile app (both register as `owner`; see `docs/modules/channels.md`),
    // so a web tab and a phone on the same session share one subscriptions map
    // + in-flight buffer. Always installed — it has no operator-facing knobs,
    // and the device-auth gate (an approved `auth_token`) is what admits a
    // device connection.
    install_channel(registry, ChannelType::owner(), core, queues)?;
    Ok(())
}

/// Install one channel with its approval gate. For the shared `owner` chat
/// channel (the merged web + mobile fan-out domain) the [`SessionPulse`]
/// observer is attached too: a `UserEcho` or a *completed* agent emission
/// (terminal `Message`/`Notice`) then emits a throttled
/// `Frame::SessionActivity` so clients not subscribed to the affected session
/// still get the unread signal. TUI is `Subscribed` as well but is
/// deliberately excluded — it ignores `SessionActivity`, and its
/// post-`Subscribe` handshake expects `HistorySnapshot` as the very next
/// frame, which a broadcast pulse could race.
pub(crate) fn install_channel(
    registry: &Arc<ChannelRegistry>,
    channel_type: ChannelType,
    core: &Arc<AskCore>,
    queues: &AskQueues,
) -> ChannelResult<()> {
    let kind = kind_for(&channel_type);
    let queue = queues.get_or_create(&channel_type);
    let channel = build_channel(channel_type.clone(), kind, queue, Arc::clone(core));
    let wants_pulse = channel_type == ChannelType::owner();
    registry.install(channel)?;
    if wants_pulse
        && let Some(chan) = registry.get(&channel_type)
        && let Some(sub) = chan.as_subscribed()
    {
        SessionPulse::new().install(sub);
    }
    Ok(())
}

/// Build a `Channel` with an ask gate over `queue` and the queue watchers that
/// turn its asks into frames. Returns an `Arc<Channel>` ready to install.
pub(crate) fn build_channel(
    channel_type: ChannelType,
    kind: ChannelKind,
    queue: AskQueue,
    core: Arc<AskCore>,
) -> Arc<Channel> {
    let approval_gate: Arc<dyn ApprovalGate> = Arc::new(ChannelAskGate::new(queue.clone(), core));
    let approvals = AskSurface {
        approval_gate,
        queue: queue.clone(),
    };
    let channel = Arc::new(Channel::new(channel_type, kind, Some(approvals)));
    let weak = Arc::downgrade(&channel);

    // One dispatch home for raise and close. The event carries the ask it is
    // FOR: reading the queue back (say, its tail) would race two sessions
    // asking at once on this per-CHANNEL queue.
    let weak_for_asks = weak.clone();
    queue.add_ask_watcher(Arc::new(move |event| {
        let Some(channel) = weak_for_asks.upgrade() else {
            return;
        };
        match event {
            AskQueueEvent::AskRaised(ask) => announce(&channel, ask),
            AskQueueEvent::AskClosed {
                session_id,
                ask_id,
                outcome,
                ..
            } => {
                if let AskResolution::Approval(decision) = outcome {
                    channel.dispatch_approval_resolved(
                        ask_id.to_string(),
                        session_id.clone(),
                        decision,
                    );
                }
                channel.dispatch_ask_resolved(session_id, ask_id, AskOutcomeKind::from(&outcome));
            }
        }
    }));
    install_session_mark(&queue, weak);
    channel
}

/// The chat-list "waiting for you" mark. Two things force it onto a
/// BROADCAST patch driven by the queue's per-session edges rather than off
/// the per-ask frames:
///
/// - `ApprovalRequested` is dispatched only to connections subscribed to the
///   ask's session, so a client parked on its conversation list — the one
///   that needs to know which chat to open — never sees it.
/// - The mark is per session: a second ask on a waiting session must not
///   re-raise it, and answering one of two must not clear it.
///
/// The queue publishes both edges (see `PendingWatcher`), in mutation order
/// and with no lock of its own held. It still runs INLINE on whichever thread
/// wins the drain — usually the blocked tool's own future — so this closure
/// must stay non-blocking.
fn install_session_mark(queue: &AskQueue, channel: Weak<Channel>) {
    queue.add_pending_watcher(Arc::new(move |session_id, edge| {
        let Some(channel) = channel.upgrade() else {
            return;
        };
        // Multiplexed channels (telegram/discord/weixin) have no session list
        // to mark, and `broadcast_session_patch` is a Subscribed-only concept.
        let Some(sub) = channel.as_subscribed() else {
            return;
        };
        sub.broadcast_session_patch(
            session_id,
            baybo_channels::wire::SessionPatch {
                awaiting_human: Some(edge.is_pending()),
                ..Default::default()
            },
        );
    }));
}

/// Fan one pending ask out to the clients that can show it.
fn announce(channel: &Channel, ask: PendingAsk) {
    let Some(entry) = ask.approval().cloned() else {
        // TODO(human-ask): dispatch `QuestionAsked` once questions exist (PR 2).
        return;
    };
    channel.dispatch_approval_requested(
        entry.call_id,
        Some(entry.tool_call_id),
        entry.session_id,
        entry.user_id,
        entry.tool,
        entry.accesses,
        entry.params_preview,
        entry.description,
    );
}

/// An ask core over an in-memory ledger, for unit tests that install
/// channels without a sqlite store.
#[cfg(test)]
pub(crate) fn test_ask_core() -> Arc<AskCore> {
    Arc::new(AskCore::new(
        baybo_human_ask::test_support::MemoryHumanAskStore::new(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core() -> Arc<AskCore> {
        test_ask_core()
    }

    #[test]
    fn install_defaults_creates_tui_channel() {
        let cfg = ChannelsConfig::default(); // cli.enabled = true
        let reg = Arc::new(ChannelRegistry::new());
        install_channels(&reg, &cfg, &core(), &AskQueues::new()).expect("install");
        assert!(reg.get(&ChannelType::tui()).is_some());
    }

    #[test]
    fn install_attaches_to_the_queue_a_reload_already_created() {
        let queues = AskQueues::new();
        let preloaded = queues.get_or_create(&ChannelType::owner());
        let reg = Arc::new(ChannelRegistry::new());
        install_channels(&reg, &ChannelsConfig::default(), &core(), &queues).expect("install");
        preloaded.enqueue_mirror(baybo_tools::ApprovalRequest {
            call_id: "c1".into(),
            tool_call_id: "use-1".into(),
            session_id: "s1".into(),
            user_id: "u".into(),
            tool: "Bash".into(),
            accesses: vec![],
            params_preview: "{}".into(),
            description: None,
        });
        let owner = reg.get(&ChannelType::owner()).expect("owner");
        assert_eq!(owner.pending_approvals(&"s1".into()).len(), 1);
    }

    #[test]
    fn kind_for_known_types() {
        assert!(kind_for(&ChannelType::owner()).is_subscribed());
        assert!(kind_for(&ChannelType::tui()).is_subscribed());
        assert!(kind_for(&ChannelType::owner()).is_subscribed());
        assert!(kind_for(&ChannelType::telegram()).is_multiplexed());
        assert!(kind_for(&ChannelType::weixin()).is_multiplexed());
        assert!(kind_for(&ChannelType::discord()).is_multiplexed());
    }

    use baybo_channels::wire::Frame;
    use baybo_channels::{Connection, ConnectionSink, SendOutcome, SessionEvent};
    use baybo_human_ask::{HumanAskManager, HumanAskManagerConfig};
    use baybo_model::human_ask::AskId;
    use baybo_tools::{ApprovalDecision, ApprovalOutcome, ApprovalRequest, AskCancel, AskSite};
    use parking_lot::Mutex;

    #[derive(Default)]
    struct Recorded {
        events: Mutex<Vec<SessionEvent>>,
        frames: Mutex<Vec<Frame>>,
    }

    struct RecordingSink(Arc<Recorded>);

    impl ConnectionSink for RecordingSink {
        fn try_send_event(&self, event: SessionEvent) -> SendOutcome {
            self.0.events.lock().push(event);
            SendOutcome::Sent
        }
        fn try_send_frame(&self, frame: Frame) -> SendOutcome {
            self.0.frames.lock().push(frame);
            SendOutcome::Sent
        }
    }

    struct Wired {
        channel: Arc<Channel>,
        asks: HumanAskManager,
        seen: Arc<Recorded>,
    }

    fn wired(session: &str) -> Wired {
        let core = core();
        let queues = AskQueues::new();
        let channel = build_channel(
            ChannelType::owner(),
            ChannelKind::Subscribed,
            queues.get_or_create(&ChannelType::owner()),
            Arc::clone(&core),
        );
        let seen = Arc::new(Recorded::default());
        let conn = Arc::new(Connection::new(Arc::new(RecordingSink(Arc::clone(&seen)))));
        channel.attach(Arc::clone(&conn));
        channel
            .as_subscribed()
            .expect("subscribed")
            .subscribe(conn.id(), session.into())
            .expect("subscribe");
        let asks = HumanAskManager::from_config(HumanAskManagerConfig {
            core,
            queues,
            resume_sink: Arc::new(baybo_human_ask::test_support::NoResume),
        });
        Wired {
            channel,
            asks,
            seen,
        }
    }

    fn ask_request(call_id: &str, session: &str) -> ApprovalRequest {
        ApprovalRequest {
            call_id: call_id.into(),
            tool_call_id: format!("use-{call_id}"),
            session_id: session.into(),
            user_id: "u".into(),
            tool: "Bash".into(),
            accesses: vec![],
            params_preview: "{}".into(),
            description: None,
        }
    }

    async fn raise(w: &Wired, call_id: &str) -> tokio::task::JoinHandle<ApprovalOutcome> {
        let gate = w.channel.approval_gate().expect("gate");
        let req = ask_request(call_id, "s1");
        let task = tokio::spawn(async move {
            gate.request(req, AskSite::for_test(), AskCancel::never())
                .await
        });
        let ask_id: AskId = call_id.parse().expect("ask id");
        while !w
            .channel
            .ask_queue()
            .is_some_and(|queue| queue.contains(&ask_id))
        {
            tokio::task::yield_now().await;
        }
        task
    }

    fn marks(seen: &Recorded) -> Vec<Option<bool>> {
        seen.frames
            .lock()
            .iter()
            .filter_map(|frame| match frame {
                Frame::SessionUpdated { patch, .. } => Some(patch.awaiting_human),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_raise_is_announced_and_an_answer_closes_it_on_every_client() {
        let w = wired("s1");
        let parked = raise(&w, "c1").await;
        assert!(matches!(
            w.seen.events.lock().as_slice(),
            [SessionEvent::ApprovalRequested { call_id, tool_call_id, .. }]
                if call_id == "c1" && tool_call_id.as_deref() == Some("use-c1")
        ));
        assert_eq!(marks(&w.seen), vec![Some(true)]);

        w.asks
            .resolve(
                &"c1".parse().expect("ask id"),
                AskResolution::Approval(ApprovalDecision::Approve),
            )
            .await
            .expect("resolve");
        assert_eq!(
            parked.await.expect("gate task"),
            ApprovalOutcome::answered(ApprovalDecision::Approve)
        );
        let events = w.seen.events.lock();
        assert!(matches!(
            &events[1..],
            [
                SessionEvent::ApprovalResolved { call_id, decision: ApprovalDecision::Approve, .. },
                SessionEvent::AskResolved { ask_id, outcome: AskOutcomeKind::Answered, .. },
            ] if call_id == "c1" && ask_id.as_str() == "c1"
        ));
        assert_eq!(marks(&w.seen), vec![Some(true), Some(false)]);
    }

    #[tokio::test]
    async fn an_abandoned_approval_still_leaves_every_client() {
        let w = wired("s1");
        let parked = raise(&w, "c2").await;
        assert!(
            w.asks
                .abandon(&"c2".parse().expect("ask id"))
                .await
                .expect("abandon")
        );
        assert_eq!(
            parked.await.expect("gate task"),
            ApprovalOutcome::abandoned()
        );
        let events = w.seen.events.lock();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::ApprovalResolved { .. })),
            "nobody decided, so there is no decision to broadcast"
        );
        assert!(matches!(
            events.last(),
            Some(SessionEvent::AskResolved { ask_id, outcome: AskOutcomeKind::Abandoned, .. })
                if ask_id.as_str() == "c2"
        ));
        assert_eq!(marks(&w.seen), vec![Some(true), Some(false)]);
    }

    #[test]
    fn build_channel_attaches_approval_gate() {
        let ch = build_channel(
            ChannelType::owner(),
            ChannelKind::Subscribed,
            AskQueue::new(),
            core(),
        );
        assert!(ch.approval_gate().is_some());
    }
}
