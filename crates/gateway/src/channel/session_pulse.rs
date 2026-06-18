//! Per-session activity broadcast throttle.
//!
//! Wired as a `DispatchObserver` on the `http` channel (see
//! [`SessionPulse::install`]). A `SessionEvent::UserEcho`, or a
//! `SessionEvent::Agent` carrying a *completed* emission (the turn's
//! terminal `Message`, or a terminal `Notice`), flowing through the
//! channel's event dispatch is collapsed by `(session_id, source)` and,
//! if the throttle window has elapsed, emits a `Frame::SessionActivity`
//! to every connection on the channel — subscribed or not. That's the
//! whole point: a sidebar tab parked on session A still wants the cheap
//! "F had activity" signal without paying for F's full AnswerDelta
//! stream.
//!
//! Mid-turn agent events (AnswerDelta / Reasoning / tool lifecycle /
//! progress / task-list / turn-state / attachment) do **not** pulse: the
//! sidebar projects each assistant pulse to a `+1` unread badge, so
//! pulsing on streaming would climb the badge one tick per throttle
//! window across a single long reply instead of landing one `+1` when the
//! reply completes.
//!
//! The two pulse streams (user / assistant) throttle independently
//! so a user sending in F doesn't suppress the immediately-following
//! agent reply pulse — the operator should see both touchpoints in
//! their sidebar.
//!
//! Create / Hide / Unhide are *not* routed through here. Those go
//! through `admin::chat::broadcast_session_patch` directly because
//! they're operator-driven, low-frequency, and ship structural patches
//! (`SessionPatch`) instead of activity pings.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_channels::wire::ActivityKind;
use aura_channels::{AgentEvent, SessionEvent, SubscribedView};
use aura_model::SessionId;
use chrono::Utc;
use dashmap::DashMap;

/// Coalescing window for the freshness pulse. Short enough that the
/// sidebar age string never looks more than a beat stale; long enough
/// that a rapid burst (several user echoes, or a terminal `Notice`
/// immediately followed by the turn's `Message`) fans out at most one
/// frame per source.
const THROTTLE_WINDOW: Duration = Duration::from_millis(1500);

#[derive(Default)]
pub struct SessionPulse {
    last_sent: DashMap<(SessionId, ActivityKind), Instant>,
}

impl SessionPulse {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Install `self` as the dispatch observer on a Subscribed
    /// channel. Taking [`SubscribedView`] (instead of `&Channel`)
    /// encodes the constraint at the call site: only Subscribed
    /// channels can carry a pulse. The closure clones an `Arc<Self>`
    /// so dropping the caller's handle is safe.
    pub fn install(self: &Arc<Self>, channel: SubscribedView<'_>) {
        let pulse = Arc::clone(self);
        channel.set_dispatch_observer(Arc::new(move |event, view| {
            pulse.observe(event, view);
        }));
    }

    /// Observer body. Inspect the event, derive the activity source,
    /// throttle, and broadcast a `Frame::SessionActivity` on hit.
    /// `pub(crate)` for direct testing; production callers go through
    /// the dispatch hook installed by [`SessionPulse::install`].
    pub(crate) fn observe(&self, event: &SessionEvent, view: SubscribedView<'_>) {
        let (session_id, source) = match event {
            SessionEvent::UserEcho(msg) => (msg.message.session_id.clone(), ActivityKind::User),
            // Only a *completed* assistant emission is a sidebar touchpoint:
            // the turn's terminal `Message`, or a terminal out-of-band `Notice`
            // (slash reply, warning). Mid-turn streaming events are skipped so
            // a long reply lands a single `+1` unread when it finishes rather
            // than climbing the badge one tick per throttle window.
            SessionEvent::Agent(out)
                if matches!(
                    out.event,
                    AgentEvent::Message(_) | AgentEvent::Notice { .. }
                ) =>
            {
                (out.session_id.clone(), ActivityKind::Assistant)
            }
            SessionEvent::Agent(_) => return,
            // Approval prompts already have their own dedicated frame
            // (`ApprovalRequested`) that reaches every subscriber to
            // the call's session. Re-emitting as activity would
            // double-signal without buying anything new for sidebar UX.
            SessionEvent::ApprovalRequested { .. } | SessionEvent::ApprovalResolved { .. } => {
                return;
            }
        };
        if session_id.as_str().is_empty() {
            // Pre-resolution UserEcho (sidecars that send a Message
            // before the session resolver has bound it) — nothing to
            // address. Skip silently rather than broadcasting an empty
            // session_id every other tab will ignore anyway.
            return;
        }
        let now = Instant::now();
        // `entry` is shard-atomic — two concurrent observe() calls for
        // the same key can't both win the "elapsed?" race and emit
        // twice.
        let should_emit = match self.last_sent.entry((session_id.clone(), source)) {
            dashmap::Entry::Occupied(mut e) => {
                if now.duration_since(*e.get()) >= THROTTLE_WINDOW {
                    e.insert(now);
                    true
                } else {
                    false
                }
            }
            dashmap::Entry::Vacant(e) => {
                e.insert(now);
                true
            }
        };
        if !should_emit {
            return;
        }
        view.broadcast_session_activity(session_id, source, Utc::now());
    }
}
