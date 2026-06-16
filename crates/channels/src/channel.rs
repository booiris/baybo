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

use aura_model::{ChannelType, ResourceAccess, SessionId};
use aura_tools::{ApprovalDecision, ApprovalGate, ApprovalQueue, ApprovalRequest};
use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use parking_lot::Mutex;

use crate::connection::{Connection, ConnectionId, SendOutcome};
use crate::error::ConnectionNotFoundError;
use crate::kind::ChannelKind;
use crate::types::{AgentEvent, AgentOutput, IncomingMessage, SessionEvent};
use crate::wire::{ActivityKind, Frame, SessionPatch};

/// Side-effect callback invoked at the top of [`Channel::dispatch_event`],
/// before any fan-out. The observer receives the event by reference
/// (so it can't take ownership and the dispatch path keeps running)
/// and a [`SubscribedView`] of the channel (so it can call back into
/// the typed broadcast path to emit derived frames). The view-typed
/// second argument is what makes "observer fired on a multiplexed
/// channel" structurally impossible — installation goes through
/// [`SubscribedView::set_dispatch_observer`], and dispatch only
/// invokes the observer after re-narrowing to a `SubscribedView`.
pub type DispatchObserver = Arc<dyn for<'a> Fn(&SessionEvent, SubscribedView<'a>) + Send + Sync>;

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
    /// Per-session replay buffer of the *in-flight* turn's progress events
    /// (reasoning / answer / tool steps), for `Subscribed` channels only. The
    /// live stream is otherwise ephemeral until the turn's assistant message
    /// persists, so a tab that subscribes mid-turn would miss everything
    /// streamed before it joined. [`SubscribedView::subscribe`] replays this
    /// to a fresh subscriber; the buffer is reset when a new turn starts and
    /// dropped when the turn ends (history then serves a reload). Consecutive
    /// answer/reasoning deltas coalesce in place so memory tracks the turn's
    /// text size, not its frame count.
    in_flight: DashMap<SessionId, Vec<SessionEvent>>,
}

/// Cap on distinct buffered entries per session (tool steps etc.; coalesced
/// answer/reasoning count as one each). A turn with more discrete steps than
/// this stops buffering the overflow — a late joiner then catches up on the
/// rest from the live stream once subscribed.
const MAX_INFLIGHT_ENTRIES: usize = 2048;

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
            in_flight: DashMap::new(),
        }
    }

    /// Update the in-flight replay buffer for a `Subscribed` channel as an
    /// event fans out. Runs regardless of whether the session has subscribers
    /// yet, so a tab that opens mid-turn as the *first* subscriber still
    /// catches up. A turn boundary resets/clears the buffer; consecutive
    /// answer/reasoning deltas coalesce into the trailing entry.
    fn note_in_flight(&self, event: &SessionEvent, session_id: &SessionId) {
        if self.kind != ChannelKind::Subscribed {
            return;
        }
        let SessionEvent::Agent(out) = event else {
            return;
        };
        match &out.event {
            // A new turn opens a fresh buffer.
            AgentEvent::TurnState {
                active: true, ..
            } => {
                self.in_flight.insert(session_id.clone(), Vec::new());
            }
            // The turn ended — its content is now durable (a reload restores
            // it from history), so drop the live buffer.
            AgentEvent::TurnState { active: false, .. } | AgentEvent::Message(_) => {
                self.in_flight.remove(session_id);
            }
            AgentEvent::AnswerDelta(_)
            | AgentEvent::Reasoning(_)
            | AgentEvent::ToolStarted { .. }
            | AgentEvent::ToolCompleted { .. }
            | AgentEvent::Status(_)
            | AgentEvent::Attachment(_) => {
                let mut buf = self.in_flight.entry(session_id.clone()).or_default();
                // Coalesce a run of same-kind text deltas into the trailing
                // entry so the buffer is bounded by text size, not delta count.
                if let Some(SessionEvent::Agent(last)) = buf.last_mut() {
                    match (&mut last.event, &out.event) {
                        (AgentEvent::AnswerDelta(acc), AgentEvent::AnswerDelta(more))
                        | (AgentEvent::Reasoning(acc), AgentEvent::Reasoning(more)) => {
                            acc.push_str(more);
                            return;
                        }
                        _ => {}
                    }
                }
                if buf.len() < MAX_INFLIGHT_ENTRIES {
                    buf.push(event.clone());
                }
            }
            _ => {}
        }
    }

    pub fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    /// Snapshot the in-flight turn's buffered progress events for `session_id`
    /// (empty if no turn is streaming, or on a non-`Subscribed` channel). The
    /// chat-history endpoint folds these into the reconstructed transcript so a
    /// tab that loads mid-turn shows the reasoning / tool steps streamed before
    /// it joined — content the live stream hasn't persisted yet.
    pub fn in_flight_events(&self, session_id: &SessionId) -> Vec<SessionEvent> {
        self.in_flight
            .get(session_id)
            .map(|b| b.clone())
            .unwrap_or_default()
    }

    pub fn kind(&self) -> ChannelKind {
        self.kind
    }

    pub fn approval_gate(&self) -> Option<Arc<dyn ApprovalGate>> {
        self.approvals.as_ref().map(|a| Arc::clone(&a.gate))
    }

    /// Snapshot the full pending [`ApprovalRequest`] entries scoped to
    /// `session_id`. Used by route layers on (re)subscribe so a
    /// reconnecting client can both
    ///   * **recover** approval cards it never received (or lost to a
    ///     full reload) by replaying the originating
    ///     `Frame::ApprovalRequested`, and
    ///   * **reconcile** locally-cached cards against the queue's
    ///     truth via [`Self::pending_approval_call_ids`] — entries
    ///     already resolved by another tab are absent from the list
    ///     and so the stale card is dropped on the client side.
    ///
    /// Returns empty when this channel has no approval surface or no
    /// entries match the session.
    pub fn pending_approvals(&self, session_id: &SessionId) -> Vec<ApprovalRequest> {
        let Some(approvals) = self.approvals.as_ref() else {
            return Vec::new();
        };
        approvals
            .queue
            .list()
            .into_iter()
            .filter(|req| req.session_id == *session_id)
            .collect()
    }

    /// Just the `call_id`s of [`Self::pending_approvals`]. Kept for
    /// the reconciliation-only path that ships `PendingApprovalsSnapshot`.
    pub fn pending_approval_call_ids(&self, session_id: &SessionId) -> Vec<String> {
        self.pending_approvals(session_id)
            .into_iter()
            .map(|req| req.call_id)
            .collect()
    }

    /// Resolve a pending approval by `call_id`. Returns the matched
    /// entry's `session_id` so the caller can follow up with
    /// [`Channel::dispatch_approval_resolved`] targeting the right
    /// subscriber set — the connection-side `ResolveApproval` frame
    /// doesn't carry `session_id` itself. `None` when no queue entry
    /// matched (or the channel has no approval surface).
    pub fn resolve_approval(&self, call_id: &str, decision: ApprovalDecision) -> Option<SessionId> {
        let approvals = self.approvals.as_ref()?;
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
    ///
    /// Touches only the buckets this connection was subscribed to. A
    /// blanket `subscriptions.retain(...)` would walk every session
    /// the channel knows about, turning a single WS close on a busy
    /// gateway into O(sessions) work — emptiness is checked per
    /// touched bucket and the drop goes through `remove_if` so a
    /// concurrent `subscribe` re-inserting in the gap can't lose its
    /// fresh entry.
    pub fn detach(&self, id: ConnectionId) {
        if let Some((_, conn)) = self.connections.remove(&id) {
            // Snapshot to avoid holding two borrows on the same map
            // while we mutate `subscriptions` below.
            let owned: Vec<SessionId> = conn.subscribed().iter().map(|s| s.clone()).collect();
            for session_id in owned {
                let now_empty = match self.subscriptions.get(&session_id) {
                    Some(entry) => {
                        entry.remove(&id);
                        entry.is_empty()
                    }
                    None => continue,
                };
                if now_empty {
                    self.subscriptions
                        .remove_if(&session_id, |_, set| set.is_empty());
                }
            }
        }
    }

    /// Fan an [`AgentOutput`] (delta / message / notice) out to every
    /// connection that should see it. Kind-agnostic — the agent
    /// router stamps this on whatever channel a session lives on, no
    /// matter the channel kind.
    pub fn dispatch_agent(&self, output: AgentOutput) {
        self.dispatch_event(SessionEvent::Agent(output));
    }

    /// Publish an `ApprovalRequested` event to subscribers of the
    /// call's session (or to every connection on a multiplexed
    /// channel). Approval surfaces are channel-agnostic; both kinds
    /// can carry one.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_approval_requested(
        &self,
        call_id: String,
        session_id: SessionId,
        user_id: String,
        tool: String,
        accesses: Vec<ResourceAccess>,
        params_preview: String,
        description: Option<String>,
    ) {
        self.dispatch_event(SessionEvent::ApprovalRequested {
            call_id,
            session_id,
            user_id,
            tool,
            accesses,
            params_preview,
            description,
        });
    }

    /// Publish an `ApprovalResolved` event so concurrent UIs drop the
    /// prompt. Caller must have already resolved the queue entry via
    /// [`Channel::resolve_approval`].
    pub fn dispatch_approval_resolved(
        &self,
        call_id: String,
        session_id: SessionId,
        decision: ApprovalDecision,
    ) {
        self.dispatch_event(SessionEvent::ApprovalResolved {
            call_id,
            session_id,
            decision,
        });
    }

    /// Kind-typed access for [`ChannelKind::Subscribed`] operations
    /// (per-session subscribe / unsubscribe, user-echo, sidebar
    /// broadcasts). Returns `None` on multiplexed channels — and that
    /// `None` is the whole point: a caller holding a [`Channel`]
    /// reference can't accidentally invoke a Subscribed-only operation
    /// on a telegram-shape channel.
    ///
    /// There is intentionally no symmetric `as_multiplexed()` —
    /// multiplexed channels have no exclusive `Channel` operations
    /// today (bot control flows through the gateway's
    /// `ChannelControlRegistry`, not `Channel`), so a typed view
    /// would be an empty placeholder. Add one when there's a real
    /// method to put on it.
    pub fn as_subscribed(&self) -> Option<SubscribedView<'_>> {
        if matches!(self.kind, ChannelKind::Subscribed) {
            Some(SubscribedView(self))
        } else {
            None
        }
    }

    /// Internal event fan-out — the raw broadcast/subscription
    /// machinery. External callers go through [`Self::dispatch_agent`]
    /// / [`Self::dispatch_approval_requested`] /
    /// [`Self::dispatch_approval_resolved`] (kind-agnostic) or
    /// through [`SubscribedView::echo_inbound`]
    /// (Subscribed-exclusive). Non-blocking. Drops the frame for any
    /// connection whose outbound queue is full (and sends a `Reset`
    /// frame to nudge the client to re-fetch history); connections
    /// whose transport is gone are detached.
    pub(crate) fn dispatch_event(&self, event: SessionEvent) {
        // Fire the pre-dispatch hook before fan-out so the observer
        // sees every event — including those that drop below because
        // no subscribers exist for the session (the activity-broadcast
        // case relies on this exact property). Observers can only be
        // installed via `SubscribedView::set_dispatch_observer`, so
        // when the slot is `Some` the channel is structurally
        // Subscribed — `as_subscribed()` is always `Some` here. The
        // narrowing produces the `SubscribedView` the observer
        // signature demands without resorting to `unsafe`.
        let observer = self.dispatch_observer.lock().clone();
        if let Some(obs) = observer
            && let Some(view) = self.as_subscribed()
        {
            obs(&event, view);
        }
        let session_id = event.session_id().clone();
        // Keep the in-flight replay buffer current — before the fan-out, and
        // before the no-subscribers early-return below, so a tab that opens
        // mid-turn as the first subscriber still catches up.
        self.note_in_flight(&event, &session_id);
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
                    // session history is the canonical record. AnswerDelta /
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

    /// Best-effort broadcast of a raw frame to every attached
    /// connection, ignoring per-session subscriptions and channel
    /// kind. Internal — external callers go through
    /// [`SubscribedView::broadcast_session_patch`] /
    /// [`SubscribedView::broadcast_session_activity`] which encode
    /// the "this frame is meaningful on Subscribed channels only"
    /// constraint in the type system. Connections whose outbound
    /// queue is full are sent a `Reset`; closed transports are
    /// detached.
    pub(crate) fn broadcast_frame(&self, frame: Frame) {
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

/// Kind-typed access to operations that only make sense on a
/// [`ChannelKind::Subscribed`] channel. Obtained via
/// [`Channel::as_subscribed`]; cheap to copy (just borrows the
/// underlying channel reference). Methods on this view cannot be
/// called against a multiplexed channel — that's the entire point.
#[derive(Copy, Clone)]
pub struct SubscribedView<'a>(&'a Channel);

impl<'a> SubscribedView<'a> {
    /// Subscribe a connection to one session. The only failure mode
    /// is [`ConnectionNotFoundError`] (the id never `attach`-ed or
    /// was already `detach`-ed) — the previous `WrongKind` variant
    /// is structurally unreachable once you hold a `SubscribedView`,
    /// and the other [`ChannelError`] variants never originate from
    /// this code path. Surfacing only the one real failure mode
    /// keeps `match` arms honest at the call site.
    pub fn subscribe(
        &self,
        id: ConnectionId,
        session_id: SessionId,
    ) -> std::result::Result<(), ConnectionNotFoundError> {
        let channel = self.0;
        let conn = channel
            .connections
            .get(&id)
            .ok_or_else(|| ConnectionNotFoundError(id.to_string()))?;
        conn.subscribed().insert(session_id.clone());
        channel
            .subscriptions
            .entry(session_id)
            .or_default()
            .insert(id);
        Ok(())
    }

    /// Unsubscribe a connection from one session. No-op if it wasn't
    /// subscribed; never errors. Empty subscription buckets are
    /// cleaned up on the next [`Channel::detach`] pass — leaving
    /// them in place here costs ~24 bytes per bucket but avoids
    /// pinging the shard on every unsubscribe.
    pub fn unsubscribe(&self, id: ConnectionId, session_id: &SessionId) {
        let channel = self.0;
        if let Some(conn) = channel.connections.get(&id) {
            conn.subscribed().remove(session_id);
        }
        if let Some(entry) = channel.subscriptions.get(session_id) {
            entry.remove(&id);
        }
    }

    /// Echo an inbound user message back out to every connection
    /// subscribed to the message's `session_id`. The receiving tab(s)
    /// render the user's own input through the same code path as
    /// agent-emitted frames, so multi-tab views stay consistent.
    ///
    /// Multiplexed channels (telegram, weixin, …) must not reach this
    /// — the SDK's `onMessage` would echo the user's input back to
    /// the upstream platform. The type system enforces it: a caller
    /// holding `Option<SubscribedView>` from a multiplexed channel
    /// gets `None`.
    pub fn echo_inbound(&self, message: IncomingMessage) {
        self.0.dispatch_event(SessionEvent::UserEcho(message));
    }

    /// Broadcast a [`Frame::SessionUpdated`] patch to every
    /// connection on this channel. Used by the admin chat API to
    /// keep concurrent web tabs' sidebars converged on create / hide
    /// / unhide / last_active changes.
    pub fn broadcast_session_patch(&self, session_id: SessionId, patch: SessionPatch) {
        self.0
            .broadcast_frame(Frame::SessionUpdated { session_id, patch });
    }

    /// Broadcast a [`Frame::SessionActivity`] pulse for sidebar
    /// freshness / unread accounting. Fan-out is best-effort and
    /// throttled by the caller (see `SessionPulse`).
    pub fn broadcast_session_activity(
        &self,
        session_id: SessionId,
        source: ActivityKind,
        at: DateTime<Utc>,
    ) {
        self.0.broadcast_frame(Frame::SessionActivity {
            session_id,
            source,
            at,
        });
    }

    /// Install (or replace) the pre-dispatch observer. Idempotent —
    /// a second call swaps the previous closure. Only Subscribed
    /// channels can carry an observer today; multiplexed channels
    /// have no use for one and the `&Channel` cb body would have
    /// nowhere to fan a derived frame.
    pub fn set_dispatch_observer(&self, observer: DispatchObserver) {
        *self.0.dispatch_observer.lock() = Some(observer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::SendOutcome;
    use crate::types::{IncomingMessage, Message as AgentMessage};
    use aura_model::{ChannelType, MessageMetadata, User};
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSink {
        events: AtomicUsize,
    }
    impl CountingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: AtomicUsize::new(0),
            })
        }
        fn count(&self) -> usize {
            self.events.load(Ordering::SeqCst)
        }
    }
    impl crate::connection::ConnectionSink for CountingSink {
        fn try_send_event(&self, _event: SessionEvent) -> SendOutcome {
            self.events.fetch_add(1, Ordering::SeqCst);
            SendOutcome::Sent
        }
        fn try_send_frame(&self, _frame: Frame) -> SendOutcome {
            SendOutcome::Sent
        }
    }

    fn fake_inbound(session_id: &str, channel: ChannelType) -> IncomingMessage {
        IncomingMessage {
            message: AgentMessage {
                id: "msg-1".into(),
                session_id: SessionId::from(session_id),
                channel: channel.clone(),
                sender: User {
                    id: "user-1".into(),
                    name: None,
                    channel,
                },
                content: vec![],
                timestamp: Utc::now(),
                reply_to: None,
                metadata: MessageMetadata::default(),
            },
            platform_msg_id: String::new(),
        }
    }

    /// Telegram-shaped channels can't even construct a `SubscribedView`
    /// — `as_subscribed()` returns `None` — so the misdirected
    /// UserEcho fan-out is unreachable by construction. The
    /// counting-sink assertion locks in that this stays true; if a
    /// future refactor re-exposes user-echo on `&Channel` directly
    /// the count check catches it.
    #[test]
    fn multiplexed_channel_has_no_subscribed_view() {
        let channel = Channel::new(
            ChannelType::from("telegram"),
            ChannelKind::Multiplexed,
            None,
        );
        let sink = CountingSink::new();
        let conn = Arc::new(Connection::new(sink.clone()));
        channel.attach(Arc::clone(&conn));

        assert!(
            channel.as_subscribed().is_none(),
            "telegram-shape (Multiplexed) channels expose no Subscribed view",
        );
        assert_eq!(
            sink.count(),
            0,
            "no fan-out happened — view-typed API made user-echo unreachable",
        );
    }

    /// Subscribed channels obtain their view and fan UserEcho through
    /// it. Cross-tab consistency works as before; just the route to
    /// the dispatch path goes through the typed view.
    #[test]
    fn echo_inbound_reaches_subscribers_via_subscribed_view() {
        let channel = Channel::new(ChannelType::http(), ChannelKind::Subscribed, None);
        let sink = CountingSink::new();
        let conn = Arc::new(Connection::new(sink.clone()));
        let conn_id = conn.id();
        channel.attach(Arc::clone(&conn));
        let view = channel.as_subscribed().expect("Subscribed view available");
        view.subscribe(conn_id, SessionId::from("session-x"))
            .expect("subscribe");
        view.echo_inbound(fake_inbound("session-x", ChannelType::http()));

        assert_eq!(
            sink.count(),
            1,
            "Subscribed channels must echo inbound to every subscriber of the session",
        );
    }

    fn agent_evt(session: &str, event: AgentEvent) -> SessionEvent {
        SessionEvent::Agent(AgentOutput {
            session_id: SessionId::from(session),
            user_id: "u".into(),
            channel: ChannelType::http(),
            event,
        })
    }

    #[test]
    fn in_flight_buffer_snapshots_the_streaming_turn() {
        let channel = Channel::new(ChannelType::http(), ChannelKind::Subscribed, None);
        let sid = "sess-mid";
        // A turn streams reasoning (two deltas) + a tool — with NO subscriber yet.
        channel.dispatch_event(agent_evt(
            sid,
            AgentEvent::TurnState {
                active: true,
                started_at: Some(Utc::now()),
            },
        ));
        channel.dispatch_event(agent_evt(sid, AgentEvent::Reasoning("weigh".into())));
        channel.dispatch_event(agent_evt(sid, AgentEvent::Reasoning("ing it".into())));
        channel.dispatch_event(agent_evt(
            sid,
            AgentEvent::ToolStarted {
                call_id: "c1".into(),
                tool: "edit".into(),
                label: None,
            },
        ));

        // The history endpoint reads this to fold the in-flight thinking into
        // a reloading tab's transcript.
        let got = channel.in_flight_events(&SessionId::from(sid));
        // Coalesced reasoning ("weighing it") + the tool step = 2 entries.
        assert_eq!(got.len(), 2, "got {got:?}");
        let (SessionEvent::Agent(a), SessionEvent::Agent(b)) = (&got[0], &got[1]) else {
            panic!("unexpected snapshot shape: {got:?}");
        };
        assert!(
            matches!(&a.event, AgentEvent::Reasoning(r) if r == "weighing it"),
            "consecutive reasoning deltas coalesce: {:?}",
            a.event
        );
        assert!(matches!(&b.event, AgentEvent::ToolStarted { call_id, .. } if call_id == "c1"));
    }

    #[test]
    fn in_flight_buffer_cleared_at_turn_end() {
        let channel = Channel::new(ChannelType::http(), ChannelKind::Subscribed, None);
        let sid = "sess-done";
        channel.dispatch_event(agent_evt(
            sid,
            AgentEvent::TurnState {
                active: true,
                started_at: Some(Utc::now()),
            },
        ));
        channel.dispatch_event(agent_evt(sid, AgentEvent::Reasoning("thinking".into())));
        channel.dispatch_event(agent_evt(
            sid,
            AgentEvent::TurnState {
                active: false,
                started_at: None,
            },
        ));

        // A tab loading AFTER the turn ended reads nothing — history serves it.
        assert!(
            channel.in_flight_events(&SessionId::from(sid)).is_empty(),
            "buffer must clear at turn end",
        );
    }

    #[test]
    fn multiplexed_channel_does_not_buffer() {
        // Only Subscribed (web) channels buffer; a telegram-shape channel must
        // never accumulate one.
        let channel = Channel::new(ChannelType::from("telegram"), ChannelKind::Multiplexed, None);
        channel.dispatch_event(agent_evt("s", AgentEvent::Reasoning("x".into())));
        assert!(channel.in_flight.is_empty(), "multiplexed channels skip the buffer");
    }
}
