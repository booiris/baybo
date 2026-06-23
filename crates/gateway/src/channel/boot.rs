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
use baybo_tools::{ApprovalGate, ApprovalQueue, ChannelApprovalGate};

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
        ChannelType::HTTP | ChannelType::TUI | ChannelType::IOS => ChannelKind::Subscribed,
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
    // HTTP is the embedded web dashboard / chat channel and is always
    // installed — it has no operator-facing knobs. The SessionPulse
    // hookup that fires `Frame::SessionActivity` lives inside
    // [`install_channel`] so the lazy-install fallback in
    // [`super::adapter::Sidecar::build`] can never miss it either.
    install_channel(registry, ChannelType::http())?;
    // The iOS companion is a `Subscribed` channel like `http`: paired devices
    // register as `ios` and self-pull threads via `Frame::Subscribe`. Always
    // installed — the device-auth gate (an approved `auth_token`) is what
    // actually admits a connection, so the waiting channel costs nothing.
    install_channel(registry, ChannelType::ios())?;
    Ok(())
}

/// Install one channel with its approval gate. For the `http` channel
/// the [`SessionPulse`] observer is attached too: a `UserEcho` or a
/// *completed* agent emission (terminal `Message`/`Notice`) then emits a
/// throttled `Frame::SessionActivity` so sidebar tabs not subscribed to
/// the affected session still get the unread signal.
pub(crate) fn install_channel(
    registry: &Arc<ChannelRegistry>,
    channel_type: ChannelType,
) -> ChannelResult<()> {
    let kind = kind_for(&channel_type);
    let channel = build_channel(channel_type.clone(), kind);
    let is_http = channel_type == ChannelType::http();
    registry.install(channel)?;
    if is_http
        && let Some(http) = registry.get(&channel_type)
        && let Some(sub) = http.as_subscribed()
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
    let queue_for_waker = queue.clone();
    // The gate's waker needs access to the channel for fan-out, but
    // the channel hasn't been constructed yet. Hand the closure a
    // `Weak<Channel>` slot we patch in once the Arc exists.
    let weak_slot: Arc<parking_lot::Mutex<std::sync::Weak<Channel>>> =
        Arc::new(parking_lot::Mutex::new(std::sync::Weak::new()));
    let weak_for_waker = Arc::clone(&weak_slot);
    let waker: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let Some(channel) = weak_for_waker.lock().upgrade() else {
            return;
        };
        let Some(entry) = queue_for_waker.list().into_iter().next_back() else {
            return;
        };
        channel.dispatch_approval_requested(
            entry.call_id,
            entry.session_id,
            entry.user_id,
            entry.tool,
            entry.accesses,
            entry.params_preview,
            entry.description,
        );
    });

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
        assert!(kind_for(&ChannelType::http()).is_subscribed());
        assert!(kind_for(&ChannelType::tui()).is_subscribed());
        assert!(kind_for(&ChannelType::ios()).is_subscribed());
        assert!(kind_for(&ChannelType::telegram()).is_multiplexed());
        assert!(kind_for(&ChannelType::weixin()).is_multiplexed());
        assert!(kind_for(&ChannelType::discord()).is_multiplexed());
    }

    #[test]
    fn build_channel_attaches_approval_gate() {
        let ch = build_channel(ChannelType::http(), ChannelKind::Subscribed);
        assert!(ch.approval_gate().is_some());
    }
}
