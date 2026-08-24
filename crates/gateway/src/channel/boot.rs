//! Eager channel installation at gateway boot.
//!
//! Walks the workspace [`ChannelsConfig`] and installs one
//! [`baybo_channels::Channel`] per enabled channel type into the shared
//! [`ChannelRegistry`]. Channels are pinned for the lifetime of the
//! gateway process — connections come and go (browser tabs, TUI
//! processes, telegram bot subprocesses) but the channel sits there
//! waiting for them.
//!
//! Each channel is born with its own per-channel [`ChannelApprovalGate`].
//! The gate's waker closes over a `Weak<Channel>` and dispatches a
//! [`SessionEvent::ApprovalRequested`] into the channel's fan-out path
//! so every subscriber to the call's session sees the prompt; the
//! first `ResolveApproval` wins and the channel publishes
//! [`SessionEvent::ApprovalResolved`] to dismiss it elsewhere.

use std::sync::Arc;
use std::time::Duration;

use baybo_channels::{
    ApprovalSurface, Channel, ChannelKind, ChannelRegistry, Result as ChannelResult,
};
use baybo_config::ChannelsConfig;
use baybo_model::{ChannelType, SessionId};
use baybo_project::{ProjectManager, TimelineApprovalGate};
use baybo_store::SessionStore;
use baybo_tools::{ApprovalGate, ApprovalQueue, ApprovalRequest, ChannelApprovalGate};

use super::session_pulse::SessionPulse;

/// Matches the previous per-Sidecar value so operator muscle memory
/// around approval timing carries over.
pub(crate) const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

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
) -> ChannelResult<()> {
    if config.cli.enabled {
        install_channel(registry, ChannelType::tui())?;
    }
    if config.telegram.as_ref().is_some_and(|c| c.enabled) {
        install_channel(registry, ChannelType::telegram())?;
    }
    if config.discord.as_ref().is_some_and(|c| c.enabled) {
        install_channel(registry, ChannelType::discord())?;
    }
    if config.weixin.as_ref().is_some_and(|c| c.enabled) {
        install_channel(registry, ChannelType::weixin())?;
    }
    // The single owner chat channel serves both the web dashboard and the
    // mobile app (both register as `owner`; see `docs/modules/channels.md`),
    // so a web tab and a phone on the same session share one subscriptions map
    // + in-flight buffer. Always installed — it has no operator-facing knobs,
    // and the device-auth gate (an approved `auth_token`) is what admits a
    // device connection.
    install_channel(registry, ChannelType::owner())?;
    Ok(())
}

/// Wrap the owner channel's approval gate so an issue run's prompts also land
/// on the card that raised them — see [`TimelineApprovalGate`]. Call once,
/// after [`install_channels`].
///
/// The gate being wrapped is read off the channel that owns it, not back out
/// of the map this is about to overwrite: the channel's gate is by
/// construction the one that prompts, so the wrapper cannot end up wrapping
/// itself or a later replacement.
/// Returns the wrapper so shutdown can await closure of parked prompts.
pub fn install_timeline_approval_gate(
    registry: &Arc<ChannelRegistry>,
    manager: Arc<ProjectManager>,
    sessions: Arc<dyn SessionStore>,
) -> Option<Arc<TimelineApprovalGate>> {
    let owner = ChannelType::owner();
    let Some(inner) = registry.get(&owner).and_then(|c| c.approval_gate()) else {
        tracing::warn!("no owner approval gate to wrap; cards will not record their prompts");
        return None;
    };
    let gate = Arc::new(TimelineApprovalGate::new(inner, manager, sessions));
    registry
        .approval_gates()
        .insert(owner, Arc::clone(&gate) as Arc<dyn ApprovalGate>);
    Some(gate)
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
) -> ChannelResult<()> {
    let kind = kind_for(&channel_type);
    let channel = build_channel(channel_type.clone(), kind);
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

/// Build a `Channel` with a per-channel approval gate whose waker
/// fan-outs `ApprovalRequested` through the channel itself. Returns
/// an `Arc<Channel>` ready to install.
pub(crate) fn build_channel(channel_type: ChannelType, kind: ChannelKind) -> Arc<Channel> {
    let queue = ApprovalQueue::new();
    // The gate's waker needs access to the channel for fan-out, but
    // the channel hasn't been constructed yet. Hand the closure a
    // `Weak<Channel>` slot we patch in once the Arc exists.
    let weak_slot: Arc<parking_lot::Mutex<std::sync::Weak<Channel>>> =
        Arc::new(parking_lot::Mutex::new(std::sync::Weak::new()));
    let weak_for_waker = Arc::clone(&weak_slot);
    // Broadcast the entry the gate woke us FOR. Reading it back off the queue
    // instead (say, the tail) is a lost-prompt race: the queue is per-CHANNEL,
    // so two sessions blocking at once interleave push/wake and one of them
    // would be announced twice while the other was never announced at all —
    // and nothing re-announces it, so that turn silently denies itself when the
    // gate times out.
    let waker: Arc<dyn Fn(ApprovalRequest) + Send + Sync> = Arc::new(move |entry| {
        let Some(channel) = weak_for_waker.lock().upgrade() else {
            return;
        };
        channel.dispatch_approval_requested(
            entry.call_id,
            entry.tool_call_id,
            entry.session_id,
            entry.user_id,
            entry.tool,
            entry.accesses,
            entry.params_preview,
            entry.description,
        );
    });

    // The chat-list "waiting for you" mark. Two things force it onto a
    // BROADCAST patch driven by the QUEUE rather than off the waker above:
    //
    // - `ApprovalRequested` is dispatched only to connections subscribed to
    //   the call's session, so a client parked on its conversation list —
    //   the one that needs to know which chat to open — never sees it.
    // - The waker fires on the way IN and has no counterpart on the way out.
    //   A gate that nobody answers self-denies after `APPROVAL_TIMEOUT` and
    //   broadcasts nothing at all, so a mark retired by `ApprovalResolved`
    //   alone would stick forever on exactly the turns that went unanswered.
    //
    // The queue publishes both edges (see `PendingWatcher`), in mutation order
    // and with no lock of its own held. It still runs INLINE on whichever
    // thread wins the drain — usually the blocked tool's own future — so this
    // closure must stay non-blocking; it just can no longer deadlock the gate.
    let weak_for_watcher = Arc::clone(&weak_slot);
    queue.add_pending_watcher(Arc::new(move |session_id, edge| {
        let Some(channel) = weak_for_watcher.lock().upgrade() else {
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
                approval_pending: Some(edge.is_pending()),
                ..Default::default()
            },
        );
    }));

    let gate: Arc<dyn ApprovalGate> = Arc::new(ChannelApprovalGate::new(
        queue.clone(),
        waker,
        APPROVAL_TIMEOUT,
    ));
    let approvals = ApprovalSurface { gate, queue };
    let channel = Arc::new(Channel::new(channel_type, kind, Some(approvals)));
    *weak_slot.lock() = Arc::downgrade(&channel);
    channel
}

/// Translate a connection-side `Frame::ApprovalResolved` decision into
/// the channel's broadcast path. Pure helper that builds the
/// `SessionEvent::ApprovalResolved` and dispatches it.
pub(crate) fn broadcast_approval_resolved(
    channel: &Channel,
    call_id: String,
    session_id: SessionId,
    decision: baybo_tools::ApprovalDecision,
) {
    channel.dispatch_approval_resolved(call_id, session_id, decision);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_defaults_creates_tui_channel() {
        let cfg = ChannelsConfig::default(); // cli.enabled = true
        let reg = Arc::new(ChannelRegistry::new());
        install_channels(&reg, &cfg).expect("install");
        assert!(reg.get(&ChannelType::tui()).is_some());
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

    #[test]
    fn build_channel_attaches_approval_gate() {
        let ch = build_channel(ChannelType::owner(), ChannelKind::Subscribed);
        assert!(ch.approval_gate().is_some());
    }
}
