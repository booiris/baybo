//! Per-WebSocket transport handle attached to a [`Channel`](crate::Channel).
//!
//! A [`Connection`] is the *transport instance*; a [`Channel`] is the
//! *protocol surface*. N connections per channel. Connections come and
//! go (open WS, close WS); channels are pinned for the lifetime of the
//! gateway process.
//!
//! The connection itself owns no socket and runs no task. The gateway
//! provides a `ConnectionSink` impl that wraps its outbound mpsc
//! channels, and the channels crate's fan-out logic talks to that
//! sink. This keeps `aura-channels` free of any wire-format or
//! transport details.

use std::sync::Arc;

use aura_model::SessionId;
use dashmap::DashSet;
use uuid::Uuid;

use crate::types::SessionEvent;
use crate::wire::Frame;

/// Identifier minted by [`Channel::attach`](crate::Channel::attach) and
/// used everywhere the registry / channel needs to address one
/// connection out of many. New `Uuid::v4` per attach. Lives entirely
/// in-process — never serialised over the wire — so it skips serde.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionId(pub Uuid);

impl ConnectionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ConnectionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// What outcomes a [`ConnectionSink`] send can produce. Either delivery
/// succeeded (`Sent`), the connection's queue is full (`Full`), or the
/// connection's transport is gone (`Closed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,
    Full,
    Closed,
}

/// The transport-side surface a [`Connection`] uses to push payloads to
/// its peer. The gateway implements this over its per-WS outbound
/// mpscs; tests can swap in an in-memory implementation.
///
/// All methods are synchronous and non-blocking — callers (the channel
/// fan-out path) cannot afford to wait on a slow consumer. A `Full`
/// return is the channel's signal to drop the payload and emit a
/// `Reset` frame to nudge the client back to a known state.
pub trait ConnectionSink: Send + Sync + 'static {
    /// Send a [`SessionEvent`] toward the peer. The transport converts
    /// it to a wire `Frame` before serialisation.
    fn try_send_event(&self, event: SessionEvent) -> SendOutcome;

    /// Send a raw wire `Frame` toward the peer. Used for control frames
    /// (`StartBot` / `StopBot` / `SlashManifest` / `Reset` / etc.) that
    /// don't pass through the session-event channel.
    fn try_send_frame(&self, frame: Frame) -> SendOutcome;
}

/// Channel-side handle for one live transport instance. The transport
/// (gateway WS route) constructs this when a peer finishes the
/// `Register` handshake, calls [`Channel::attach`](crate::Channel::attach)
/// with the `Arc<Connection>`, and drops it on WS close.
pub struct Connection {
    id: ConnectionId,
    sink: Arc<dyn ConnectionSink>,
    /// `session_id`s this connection is currently subscribed to. For
    /// `ChannelKind::Multiplexed` channels the channel doesn't read
    /// this (fan-out iterates `connections`); for `Subscribed` channels
    /// it's the canonical record of interest, mirrored into the
    /// channel's reverse index for O(subscribers) lookups.
    subscribed: DashSet<SessionId>,
}

impl Connection {
    pub fn new(sink: Arc<dyn ConnectionSink>) -> Self {
        Self {
            id: ConnectionId::new(),
            sink,
            subscribed: DashSet::new(),
        }
    }

    pub fn id(&self) -> ConnectionId {
        self.id
    }

    pub(crate) fn sink(&self) -> &Arc<dyn ConnectionSink> {
        &self.sink
    }

    pub(crate) fn subscribed(&self) -> &DashSet<SessionId> {
        &self.subscribed
    }

    /// True if the connection is currently subscribed to `session_id`.
    pub fn is_subscribed_to(&self, session_id: &SessionId) -> bool {
        self.subscribed.contains(session_id)
    }
}
