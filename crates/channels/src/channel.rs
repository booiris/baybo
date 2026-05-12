//! Concrete channel handle stored in [`ChannelRegistry`].
//!
//! A `Channel` is a *protocol surface* (telegram, weixin, tui, http,
//! …) — 1:1 with `ChannelType`. It owns its
//! [`Connection`]s (N per channel, transport-provided) and, for
//! `Subscribed` kinds, the reverse index from `session_id` to the
//! subscribed connections. Agent output is dispatched into the channel
//! and fanned out per the channel's [`ChannelKind`]; the channel never
//! touches the wire.

use std::sync::Arc;

use aura_model::{ChannelType, SessionId};
use aura_tools::{ApprovalDecision, ApprovalGate, ApprovalQueue};
use dashmap::{DashMap, DashSet};
use parking_lot::Mutex;

use crate::connection::{Connection, ConnectionId, SendOutcome};
use crate::kind::ChannelKind;
use crate::types::{IncomingMessage, SessionEvent};
use crate::wire::Frame;
use crate::{ChannelError, Result};

/// Side-effect callback invoked at the top of [`Channel::dispatch`],
/// before any fan-out. The observer receives the event by reference
/// (so it can't take ownership and the dispatch path keeps running)
/// and the channel itself (so it can call back into
/// [`Channel::broadcast_frame`] to emit derived frames). Used by the
/// gateway to fire `Frame::SessionActivity` broadcasts for the
/// `http` channel without coupling this crate to web-sidebar concerns.
pub type DispatchObserver = Arc<dyn Fn(&SessionEvent, &Channel) + Send + Sync>;

/// Bundle of the channel's approval gate (the `ApprovalGate` trait
/// object the agent / tool path calls through) and the underlying
/// queue (used to resolve pending entries by call id when a client
/// sends `Frame::ResolveApproval`). Both share the same queue handle
/// internally; this struct just keeps the second one externally
/// addressable from the channel.
pub struct ApprovalSurface {
    pub gate: Arc<dyn ApprovalGate>,
    pub queue: ApprovalQueue,
}

/// Live protocol surface. One per [`ChannelType`]. Created eagerly at
/// gateway boot from `ChannelsConfig`; never dropped while the gateway
/// is up.
pub struct Channel {
    channel_type: ChannelType,
    kind: ChannelKind,
    approvals: Option<ApprovalSurface>,
    connections: DashMap<ConnectionId, Arc<Connection>>,
    /// Reverse index for `Subscribed` channels: `session_id` →
    /// connections that asked to see it. Untouched for `Multiplexed`.
    subscriptions: DashMap<SessionId, DashSet<ConnectionId>>,
    /// Optional pre-dispatch hook. Set by the gateway on the `http`
    /// channel after install to fire `Frame::SessionActivity`
    /// broadcasts; other channels leave it `None`. Locked only long
    /// enough to clone the `Arc` — the observer body runs without the
    /// lock so it can re-enter `Channel::broadcast_frame` safely.
    dispatch_observer: Mutex<Option<DispatchObserver>>,
}

impl Channel {
    pub fn new(
        channel_type: ChannelType,
        kind: ChannelKind,
        approvals: Option<ApprovalSurface>,
    ) -> Self {
        Self {
            channel_type,
            kind,
            approvals,
            connections: DashMap::new(),
            subscriptions: DashMap::new(),
            dispatch_observer: Mutex::new(None),
        }
    }

    /// Install (or replace) the pre-dispatch observer. Idempotent —
    /// a second call swaps the previous closure. Pass-through callers
    /// don't need this; only the http-channel pulse wiring uses it.
    pub fn set_dispatch_observer(&self, observer: DispatchObserver) {
        *self.dispatch_observer.lock() = Some(observer);
    }

    pub fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    pub fn kind(&self) -> ChannelKind {
        self.kind
    }

    pub fn approval_gate(&self) -> Option<Arc<dyn ApprovalGate>> {
        self.approvals.as_ref().map(|a| Arc::clone(&a.gate))
    }

    /// Snapshot the `call_id`s of currently-pending approvals scoped to
    /// `session_id`. Used by route layers on (re)subscribe to ship a
    /// reconciliation frame so clients can drop locally-cached prompt
    /// cards whose underlying approvals were resolved while their
    /// connection was down — `Frame::ApprovalResolved` is in-band
    /// fan-out only, not persisted, not replayed on catch-up. Returns
    /// empty when this channel has no approval surface (the queue is
    /// optional per channel) or no entries match the session.
    pub fn pending_approval_call_ids(&self, session_id: &SessionId) -> Vec<String> {
        let Some(approvals) = self.approvals.as_ref() else {
            return Vec::new();
        };
        approvals
            .queue
            .list()
            .into_iter()
            .filter(|req| req.session_id == *session_id)
            .map(|req| req.call_id)
            .collect()
    }

    /// Resolve a pending approval by `call_id`. Returns `true` if the
    /// queue contained a matching entry and it was resolved. Caller is
    /// expected to follow up with [`Channel::dispatch`] of
    /// `SessionEvent::ApprovalResolved` so concurrent subscribers see
    /// the dismissal.
    pub fn resolve_approval(&self, call_id: &str, decision: ApprovalDecision) -> bool {
        let Some(approvals) = self.approvals.as_ref() else {
            return false;
        };
        approvals.queue.resolve_by_call_id(call_id, decision)
    }

    /// Attach a transport-provided connection. Idempotent on the
    /// connection's `id` (a duplicate id swap-replaces the old entry —
    /// callers mint fresh ids per attach, so a hit here is a
    /// programming error rather than expected concurrency).
    pub fn attach(&self, conn: Arc<Connection>) {
        self.connections.insert(conn.id(), conn);
    }

    /// Detach by id. Also drops every subscription this connection
    /// owned so the reverse index doesn't leak.
    pub fn detach(&self, id: ConnectionId) {
        if let Some((_, conn)) = self.connections.remove(&id) {
            // Snapshot to avoid holding two borrows on the same map
            // while we mutate `subscriptions` below.
            let owned: Vec<SessionId> = conn.subscribed().iter().map(|s| s.clone()).collect();
            for session_id in owned {
                if let Some(entry) = self.subscriptions.get(&session_id) {
                    entry.remove(&id);
                }
            }
            // Drop now-empty subscription buckets so the map shrinks.
            self.subscriptions.retain(|_, set| !set.is_empty());
        }
    }

    /// Subscribe a connection to one session. `Subscribed`-kind only.
    /// Returns `ChannelError::WrongKind` if the channel is `Multiplexed`,
    /// and `ChannelError::ConnectionNotFound` if the id never attached.
    pub fn subscribe(&self, id: ConnectionId, session_id: SessionId) -> Result<()> {
        if self.kind.is_multiplexed() {
            return Err(ChannelError::WrongKind {
                channel_type: self.channel_type.to_string(),
                expected: ChannelKind::Subscribed,
                actual: ChannelKind::Multiplexed,
            });
        }
        let conn = self
            .connections
            .get(&id)
            .ok_or_else(|| ChannelError::ConnectionNotFound(id.to_string()))?;
        conn.subscribed().insert(session_id.clone());
        self.subscriptions.entry(session_id).or_default().insert(id);
        Ok(())
    }

    /// Unsubscribe a connection from one session. No-op if it wasn't
    /// subscribed; never errors.
    pub fn unsubscribe(&self, id: ConnectionId, session_id: &SessionId) {
        if let Some(conn) = self.connections.get(&id) {
            conn.subscribed().remove(session_id);
        }
        if let Some(entry) = self.subscriptions.get(session_id) {
            entry.remove(&id);
        }
        // Don't bother shrinking — empty buckets cost ~24 bytes; they
        // get cleaned up on the next detach pass.
    }

    /// Dispatch a [`SessionEvent`] to every connection that should see
    /// it. Non-blocking. Drops the frame for any connection whose
    /// outbound queue is full (and sends a `Reset` frame to nudge the
    /// client to re-fetch history); connections whose transport is
    /// gone are detached.
    pub fn dispatch(&self, event: SessionEvent) {
        // Fire the pre-dispatch hook before fan-out so the observer
        // sees every event — including those that drop below because
        // no subscribers exist for the session (the activity-broadcast
        // case relies on this exact property).
        let observer = self.dispatch_observer.lock().clone();
        if let Some(obs) = observer {
            obs(&event, self);
        }
        let session_id = event.session_id().clone();
        let mut to_drop = Vec::new();
        let mut to_reset = Vec::new();

        match self.kind {
            ChannelKind::Multiplexed => {
                for entry in self.connections.iter() {
                    let conn = entry.value();
                    match conn.sink().try_send_event(event.clone()) {
                        SendOutcome::Sent => {}
                        SendOutcome::Full => to_reset.push(conn.id()),
                        SendOutcome::Closed => to_drop.push(conn.id()),
                    }
                }
            }
            ChannelKind::Subscribed => {
                let Some(subs) = self.subscriptions.get(&session_id) else {
                    // Drop ephemeral: the storage layer persists
                    // Messages before this dispatch site, so the
                    // session history is the canonical record. Delta /
                    // Notice / ApprovalRequested are advisory and
                    // recoverable via REST.
                    return;
                };
                for conn_id in subs.iter() {
                    let id = *conn_id.key();
                    let Some(conn) = self.connections.get(&id) else {
                        // Subscription pointed at a connection that
                        // detached without cleaning its row; skip.
                        continue;
                    };
                    match conn.sink().try_send_event(event.clone()) {
                        SendOutcome::Sent => {}
                        SendOutcome::Full => to_reset.push(id),
                        SendOutcome::Closed => to_drop.push(id),
                    }
                }
            }
        }

        for id in to_reset {
            self.send_reset(id, "outbound queue full");
        }
        for id in to_drop {
            self.detach(id);
        }
    }

    /// Echo an inbound user message to every connection subscribed to
    /// the message's `session_id`. The receiving tab(s) render the user
    /// message through the same code path as agent-emitted frames so
    /// multi-tab views stay consistent.
    pub fn echo_inbound(&self, message: IncomingMessage) {
        self.dispatch(SessionEvent::UserEcho(message));
    }

    /// Best-effort broadcast of a raw frame to every attached
    /// connection, ignoring per-session subscriptions and channel
    /// kind. For non-session-scoped events that every client of a
    /// channel should see — today the web chat sidebar's
    /// `Frame::ChatSessionListChanged` pulse so concurrent tabs (on
    /// the same machine or across devices) refresh their session list
    /// without polling. Connections whose outbound queue is full are
    /// sent a `Reset`; closed transports are detached.
    pub fn broadcast_frame(&self, frame: Frame) {
        let mut to_drop = Vec::new();
        let mut to_reset = Vec::new();
        for entry in self.connections.iter() {
            let conn = entry.value();
            match conn.sink().try_send_frame(frame.clone()) {
                SendOutcome::Sent => {}
                SendOutcome::Full => to_reset.push(conn.id()),
                SendOutcome::Closed => to_drop.push(conn.id()),
            }
        }
        for id in to_reset {
            self.send_reset(id, "outbound queue full");
        }
        for id in to_drop {
            self.detach(id);
        }
    }

    /// True if any connection is currently subscribed to `session_id`
    /// (or, for `Multiplexed` channels, if any connection is attached
    /// at all). Useful for diagnostics; the dispatch path consults the
    /// same data without going through this helper.
    pub fn has_subscribers(&self, session_id: &SessionId) -> bool {
        match self.kind {
            ChannelKind::Multiplexed => !self.connections.is_empty(),
            ChannelKind::Subscribed => self
                .subscriptions
                .get(session_id)
                .is_some_and(|s| !s.is_empty()),
        }
    }

    /// Number of currently-attached connections.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    fn send_reset(&self, id: ConnectionId, reason: &str) {
        if let Some(conn) = self.connections.get(&id) {
            // Best-effort: if the reset itself can't enqueue (transport
            // dropped while we were iterating) we'll catch the close
            // on the next dispatch round.
            let _ = conn.sink().try_send_frame(Frame::Reset {
                reason: reason.to_owned(),
            });
        }
    }
}
