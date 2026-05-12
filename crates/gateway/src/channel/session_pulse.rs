//! Per-session `last_active` broadcast throttle.
//!
//! The inbound message loop emits a [`Frame::SessionUpdated`] patch
//! every time a user types in a chat session, so sibling tabs can
//! rerender their sidebar's `relativeAge(last_active)` without a list
//! refetch. A busy session (back-to-back agent turns, paste-bombs of
//! short messages, …) would otherwise fan out one frame per inbound;
//! this throttle coalesces them so each open chat tab sees at most
//! one patch per [`THROTTLE_WINDOW`] per session.
//!
//! Single-source-of-truth note: only the per-inbound `last_active`
//! emit routes through here. Create / Hide / Unhide go through
//! `admin::chat::broadcast_session_patch` directly because those are
//! operator-driven and not subject to rate limits.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_channels::ChannelRegistry;
use aura_channels::wire::{Frame, SessionPatch};
use aura_model::{ChannelType, SessionId};
use chrono::{DateTime, Utc};
use dashmap::DashMap;

/// Coalescing window. Picked at the low end of the user-visible
/// "1-2 s" range — short enough that the sidebar age string never
/// looks more than a beat stale, long enough that a five-message
/// burst on the same session fans out exactly one frame.
const THROTTLE_WINDOW: Duration = Duration::from_millis(1500);

#[derive(Default)]
pub struct SessionPulse {
    last_sent: DashMap<SessionId, Instant>,
}

impl SessionPulse {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Push a `last_active` patch to every connection on the `http`
    /// channel iff the per-session throttle window has elapsed.
    /// No-op when the `http` channel isn't installed.
    pub fn touch(
        &self,
        registry: &ChannelRegistry,
        session_id: &SessionId,
        last_active: DateTime<Utc>,
    ) {
        let now = Instant::now();
        // `entry` is atomic w.r.t. the DashMap shard so two concurrent
        // touches for the same session_id can't both observe the
        // pre-write deadline as "elapsed" and emit twice.
        let should_emit = match self.last_sent.entry(session_id.clone()) {
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
        let Some(channel) = registry.get(&ChannelType::http()) else {
            return;
        };
        channel.broadcast_frame(Frame::SessionUpdated {
            session_id: session_id.clone(),
            patch: SessionPatch {
                last_active: Some(last_active),
                ..SessionPatch::default()
            },
        });
    }
}
