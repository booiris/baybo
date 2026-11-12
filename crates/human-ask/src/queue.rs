//! [`AskQueue`]: the in-memory read model of a channel's pending asks.
//!
//! One queue per channel, not per session. Holds an entry for every
//! `pending` ledger row the process knows about: **live** entries carry the
//! oneshot a parked tool future waits on, **dead** entries (after a restart,
//! or once the waiter is gone) carry none and are still shown and
//! answerable. Display-only mirrors (the TUI) are dead entries too.
//!
//! The queue never touches the ledger. Watchers are sync, run outside the
//! queue lock in mutation order, and must not block.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use baybo_model::approval::{ApprovalDecision, ApprovalRequest};
use baybo_model::human_ask::{
    ASK_USER_TOOL_NAME, AskId, AskKind, AskPhase, AskRequest, AskResolution, resumable,
};
use baybo_model::{ChannelType, SessionId};
use baybo_store::HumanAskRow;
use parking_lot::Mutex;
use tokio::sync::oneshot;

/// One pending ask as the queue (and its watchers) see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsk {
    pub ask_id: AskId,
    pub session_id: SessionId,
    pub tool_call_id: String,
    pub kind: AskKind,
    pub phase: AskPhase,
    pub request: AskRequest,
}

impl PendingAsk {
    pub fn from_row(row: &HumanAskRow) -> Self {
        Self {
            ask_id: row.ask_id.clone(),
            session_id: row.session_id.clone(),
            tool_call_id: row.tool_call_id.clone(),
            kind: row.kind,
            phase: row.phase,
            request: row.request.clone(),
        }
    }

    /// The tool the ask blocks, for notifications.
    pub fn tool(&self) -> &str {
        match &self.request {
            AskRequest::Approval(req) => &req.tool,
            AskRequest::Question(_) => ASK_USER_TOOL_NAME,
        }
    }

    pub fn approval(&self) -> Option<&ApprovalRequest> {
        match &self.request {
            AskRequest::Approval(req) => Some(req),
            AskRequest::Question(_) => None,
        }
    }

    pub fn is_resumable(&self) -> bool {
        resumable(self.kind, self.phase)
    }
}

/// A queue slot: the ask plus, while a tool future waits on it, its oneshot.
/// `None` is a dead ask or a display-only mirror.
struct Entry {
    ask: PendingAsk,
    responder: Option<oneshot::Sender<AskResolution>>,
}

/// Callback fired when a dead entry is resolved locally through
/// [`AskQueue::resolve_head`]. The TUI installs one so its local resolution
/// also reaches the gateway. Runs synchronously; spawn for async work.
pub type ResolveFn = Arc<dyn Fn(AskId, AskResolution) + Send + Sync>;

/// Which way a session crossed the "has at least one pending ask" boundary,
/// and — on the way out — whether a human was behind it.
///
/// [`Self::Answered`] means the user is present and acting, so the next ask
/// deserves to interrupt them again immediately; [`Self::Abandoned`] means
/// nobody came, and re-announcing at the same volume is how an agent that
/// keeps retrying turns into an all-night stream of notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingEdge {
    /// The session's FIRST ask just parked. Carries the ask's identity so a
    /// watcher never has to read the queue back to find out what woke it.
    Raised {
        ask_id: AskId,
        kind: AskKind,
        tool: String,
    },
    /// Its LAST ask was answered by a decision someone made.
    Answered,
    /// Its LAST ask went away with no decision.
    Abandoned,
}

impl PendingEdge {
    /// Whether the session is waiting on the user after this edge.
    pub fn is_pending(&self) -> bool {
        matches!(self, PendingEdge::Raised { .. })
    }
}

/// Fired when a session crosses the "has at least one pending ask"
/// boundary. **Edge-only**: a second ask on an already-waiting session is
/// silent, and resolving one of two leaves the session waiting.
///
/// Runs with **no queue lock held**, so a watcher may read the queue back.
/// It still runs INLINE, on whichever thread wins the drain — usually a
/// parked tool's own future — and that thread keeps draining while others
/// enqueue, so it can be conscripted into publishing *other* sessions'
/// edges. Keep it non-blocking: no sqlite, no await.
///
/// Order is preserved: edges are appended to the outbox under the queue
/// lock, so their order is the order the mutations happened, and only one
/// thread drains at a time. That ordering is load-bearing — a stale `false`
/// overtaking the `true` that superseded it would strand the flag inverted.
///
/// A watcher that MUTATES the queue re-enters the drain, which returns
/// immediately (the active drainer publishes what the mutation enqueued), so
/// that is safe too — but its own edge lands after the one it is handling.
pub type PendingWatcher = Arc<dyn Fn(SessionId, PendingEdge) + Send + Sync>;

/// Per-ask sibling of [`PendingEdge`]: every ask that enters or leaves the
/// queue, for frame dispatch. Carries the session because delivery keys on
/// it and the watcher may read neither the row nor the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskQueueEvent {
    AskRaised(PendingAsk),
    AskClosed {
        session_id: SessionId,
        ask_id: AskId,
        kind: AskKind,
        outcome: AskResolution,
    },
}

/// Same contract as [`PendingWatcher`]: sync, inline, non-blocking, in
/// mutation order.
pub type AskWatcher = Arc<dyn Fn(AskQueueEvent) + Send + Sync>;

/// One thing a mutation queued for publication.
enum Outgoing {
    Edge(SessionId, PendingEdge),
    Ask(AskQueueEvent),
}

/// What [`AskQueue::resolve`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub ask: PendingAsk,
    /// A live waiter received the resolution. `false` for a dead entry, or a
    /// live one whose waiter vanished in the same instant.
    pub delivered: bool,
}

/// Thread-safe queue of pending asks. Cloneable — gate, manager and channel
/// all hold a handle.
#[derive(Clone)]
pub struct AskQueue {
    inner: Arc<Mutex<VecDeque<Entry>>>,
    resolver: Arc<Mutex<Option<ResolveFn>>>,
    pending_watchers: Arc<Mutex<Arc<Vec<PendingWatcher>>>>,
    ask_watchers: Arc<Mutex<Arc<Vec<AskWatcher>>>>,
    /// Publications awaiting the drain, in the order their mutations
    /// happened. Appending here is the only thing a mutation does while
    /// holding `inner`; the drain happens afterwards, outside the lock.
    outbox: Arc<Mutex<VecDeque<Outgoing>>>,
    /// Whether a thread is currently draining `outbox`. At most one may,
    /// which is what keeps two drains from inverting the outbox's order.
    publishing: Arc<AtomicBool>,
}

impl AskQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
            resolver: Arc::new(Mutex::new(None)),
            pending_watchers: Arc::new(Mutex::new(Arc::new(Vec::new()))),
            ask_watchers: Arc::new(Mutex::new(Arc::new(Vec::new()))),
            outbox: Arc::new(Mutex::new(VecDeque::new())),
            publishing: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Add a [`PendingWatcher`]. **Appends** — watchers are installed at
    /// different moments in boot, and a replace-style slot would silently
    /// leave one of them with no signal. Fired in install order.
    pub fn add_pending_watcher(&self, watcher: PendingWatcher) {
        let mut slot = self.pending_watchers.lock();
        let mut next = Vec::clone(slot.as_ref());
        next.push(watcher);
        *slot = Arc::new(next);
    }

    /// Add an [`AskWatcher`]. Appends, like [`Self::add_pending_watcher`].
    pub fn add_ask_watcher(&self, watcher: AskWatcher) {
        let mut slot = self.ask_watchers.lock();
        let mut next = Vec::clone(slot.as_ref());
        next.push(watcher);
        *slot = Arc::new(next);
    }

    /// Queue `session_id`'s boundary crossing if the caller's mutation caused
    /// one. `q` is the **still-locked** queue with the mutation applied;
    /// `was_pending` is what the caller observed before mutating.
    fn note_pending_edge(
        &self,
        q: &VecDeque<Entry>,
        session_id: &SessionId,
        was_pending: bool,
        edge: PendingEdge,
    ) {
        let pending = q.iter().any(|e| &e.ask.session_id == session_id);
        if pending == was_pending {
            return;
        }
        debug_assert_eq!(
            pending,
            edge.is_pending(),
            "caller labelled the edge in the opposite direction to the queue"
        );
        self.outbox
            .lock()
            .push_back(Outgoing::Edge(session_id.clone(), edge));
    }

    fn note_ask_event(&self, event: AskQueueEvent) {
        self.outbox.lock().push_back(Outgoing::Ask(event));
    }

    /// Drain the outbox into the watchers. **Call only with the queue lock
    /// released.** A second caller arriving mid-drain returns at once: the
    /// active drainer picks up what it enqueued.
    fn publish(&self) {
        /// Releases the drain claim on the unwind path too, so a panicking
        /// watcher merely delays the remaining publications to the next
        /// mutation instead of wedging every future drain.
        struct DrainClaim<'a>(&'a AtomicBool);
        impl Drop for DrainClaim<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }

        if self.publishing.swap(true, Ordering::AcqRel) {
            return;
        }
        loop {
            {
                let _claim = DrainClaim(&self.publishing);
                loop {
                    let next = self.outbox.lock().pop_front();
                    let Some(next) = next else {
                        break;
                    };
                    match next {
                        Outgoing::Edge(session_id, edge) => {
                            let watchers = Arc::clone(&*self.pending_watchers.lock());
                            for watcher in watchers.iter() {
                                watcher(session_id.clone(), edge.clone());
                            }
                        }
                        Outgoing::Ask(event) => {
                            let watchers = Arc::clone(&*self.ask_watchers.lock());
                            for watcher in watchers.iter() {
                                watcher(event.clone());
                            }
                        }
                    }
                }
            }
            if self.outbox.lock().is_empty() || self.publishing.swap(true, Ordering::AcqRel) {
                return;
            }
        }
    }

    /// Sessions with at least one pending ask, from one pass.
    pub fn pending_sessions(&self) -> HashSet<SessionId> {
        self.inner
            .lock()
            .iter()
            .map(|e| e.ask.session_id.clone())
            .collect()
    }

    /// Whether a tool future in this process is parked on an ask of
    /// `session_id`.
    pub fn has_live(&self, session_id: &SessionId) -> bool {
        self.inner
            .lock()
            .iter()
            .any(|e| &e.ask.session_id == session_id && e.responder.is_some())
    }

    /// `Some(true)` for a live entry, `Some(false)` for a dead one, `None`
    /// when the ask is not on this queue.
    pub fn liveness(&self, ask_id: &AskId) -> Option<bool> {
        self.inner
            .lock()
            .iter()
            .find(|e| &e.ask.ask_id == ask_id)
            .map(|e| e.responder.is_some())
    }

    pub fn contains(&self, ask_id: &AskId) -> bool {
        self.liveness(ask_id).is_some()
    }

    /// Install the callback [`Self::resolve_head`] runs for a dead entry.
    /// Replaces any previous one.
    pub fn set_resolver(&self, resolver: ResolveFn) {
        *self.resolver.lock() = Some(resolver);
    }

    fn resolver(&self) -> Option<ResolveFn> {
        self.resolver.lock().clone()
    }

    fn push(&self, ask: PendingAsk, responder: Option<oneshot::Sender<AskResolution>>) {
        let session_id = ask.session_id.clone();
        let raised = PendingEdge::Raised {
            ask_id: ask.ask_id.clone(),
            kind: ask.kind,
            tool: ask.tool().to_owned(),
        };
        {
            let mut q = self.inner.lock();
            let was_pending = q.iter().any(|e| e.ask.session_id == session_id);
            q.push_back(Entry {
                ask: ask.clone(),
                responder,
            });
            self.note_pending_edge(&q, &session_id, was_pending, raised);
            self.note_ask_event(AskQueueEvent::AskRaised(ask));
        }
        self.publish();
    }

    /// Enqueue an ask a tool future is about to wait on.
    pub(crate) fn push_live(&self, ask: PendingAsk, responder: oneshot::Sender<AskResolution>) {
        self.push(ask, Some(responder));
    }

    /// Enqueue an ask no tool future waits on (boot reload).
    pub(crate) fn push_dead(&self, ask: PendingAsk) {
        self.push(ask, None);
    }

    /// Take `ask_id` out of the queue, publishing `edge` if the session's
    /// last ask left and `AskClosed { outcome }`.
    fn remove(&self, ask_id: &AskId, edge: PendingEdge, outcome: AskResolution) -> Option<Entry> {
        let removed = {
            let mut q = self.inner.lock();
            let pos = q.iter().position(|e| &e.ask.ask_id == ask_id)?;
            let removed = q.remove(pos)?;
            self.note_pending_edge(&q, &removed.ask.session_id, true, edge);
            self.note_ask_event(AskQueueEvent::AskClosed {
                session_id: removed.ask.session_id.clone(),
                ask_id: removed.ask.ask_id.clone(),
                kind: removed.ask.kind,
                outcome,
            });
            removed
        };
        self.publish();
        Some(removed)
    }

    /// Remove an ask a human (or policy) decided and hand the decision to
    /// its waiter, if it has one. `None` when the ask is not queued.
    pub fn resolve(&self, ask_id: &AskId, resolution: AskResolution) -> Option<Removed> {
        let edge = if resolution == AskResolution::Abandoned {
            PendingEdge::Abandoned
        } else {
            PendingEdge::Answered
        };
        let entry = self.remove(ask_id, edge, resolution.clone())?;
        // Outside the lock: the oneshot wakes the parked tool future, which
        // may re-enter the queue on another worker.
        let delivered = entry
            .responder
            .is_some_and(|responder| responder.send(resolution).is_ok());
        Some(Removed {
            ask: entry.ask,
            delivered,
        })
    }

    /// Remove an ask nobody decided. A live waiter sees
    /// [`AskResolution::Abandoned`].
    pub fn abandon(&self, ask_id: &AskId) -> Option<Removed> {
        self.resolve(ask_id, AskResolution::Abandoned)
    }

    /// A waiter went away without a resolution. A resumable ask stays queued
    /// as a dead entry with no edge — the row is still pending and the ask
    /// still answerable. A non-resumable one is useless without its tool
    /// future, so it is removed with an `Abandoned` edge; its row is
    /// reconciled later by the manager.
    pub fn orphan(&self, ask_id: &AskId) {
        let resumable_found = {
            let mut q = self.inner.lock();
            match q.iter_mut().find(|e| &e.ask.ask_id == ask_id) {
                Some(entry) if entry.ask.is_resumable() => {
                    entry.responder = None;
                    true
                }
                Some(_) => false,
                None => return,
            }
        };
        if !resumable_found {
            let _ = self.remove(ask_id, PendingEdge::Abandoned, AskResolution::Abandoned);
        }
    }

    /// Snapshot of every queued ask, in insertion order.
    pub fn list(&self) -> Vec<PendingAsk> {
        self.inner.lock().iter().map(|e| e.ask.clone()).collect()
    }

    /// Snapshot of the queued approval requests, in insertion order.
    pub fn list_approvals(&self) -> Vec<ApprovalRequest> {
        self.inner
            .lock()
            .iter()
            .filter_map(|e| e.ask.approval().cloned())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // --- approval-typed surface for display mirrors (TUI, `baybo prompt`) ---

    /// Append a display-only approval card. The ask id is the request's
    /// `call_id`.
    pub fn enqueue_mirror(&self, req: ApprovalRequest) {
        let ask = PendingAsk {
            ask_id: mirror_id(&req.call_id),
            session_id: req.session_id.clone(),
            tool_call_id: req.tool_call_id.clone(),
            kind: AskKind::Approval,
            phase: AskPhase::BeforeExecute,
            request: AskRequest::Approval(req),
        };
        self.push_dead(ask);
    }

    /// The head entry's approval request, `None` when the queue is empty or
    /// the head is another kind.
    pub fn peek_head_approval(&self) -> Option<ApprovalRequest> {
        self.inner
            .lock()
            .front()
            .and_then(|e| e.ask.approval().cloned())
    }

    /// Resolve the head entry with an approval decision. A live head's waiter
    /// receives it; a dead head goes to the installed [`ResolveFn`]. `false`
    /// when the queue is empty.
    pub fn resolve_head(&self, decision: ApprovalDecision) -> bool {
        let Some(head) = self.inner.lock().front().map(|e| e.ask.ask_id.clone()) else {
            return false;
        };
        let resolution = AskResolution::Approval(decision);
        let Some(removed) = self.resolve(&head, resolution.clone()) else {
            return false;
        };
        if !removed.delivered
            && let Some(resolver) = self.resolver()
        {
            resolver(removed.ask.ask_id, resolution);
        }
        true
    }

    /// Remove the entry with `call_id` without deciding it — another client
    /// resolved it. `false` when no such entry is queued.
    pub fn drop_call(&self, call_id: &str) -> bool {
        self.abandon(&mirror_id(call_id)).is_some()
    }
}

impl Default for AskQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// A mirror's ask id is its wire `call_id`. An empty one (never sent in
/// practice) gets a fresh id, so it can be shown but not dropped by id.
fn mirror_id(call_id: &str) -> AskId {
    call_id.parse().unwrap_or_else(|_| AskId::new())
}

/// One [`AskQueue`] per channel. The channel install and the boot reload both
/// go through [`Self::get_or_create`], so a reload for a channel whose
/// sidecar has not connected yet creates the queue the later install
/// attaches to. A few operations per boot or connect — a plain mutex.
#[derive(Clone, Default)]
pub struct AskQueues {
    inner: Arc<Mutex<HashMap<ChannelType, AskQueue>>>,
}

impl AskQueues {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_create(&self, channel: &ChannelType) -> AskQueue {
        self.inner
            .lock()
            .entry(channel.clone())
            .or_default()
            .clone()
    }

    pub fn get(&self, channel: &ChannelType) -> Option<AskQueue> {
        self.inner.lock().get(channel).cloned()
    }

    pub(crate) fn all(&self) -> Vec<AskQueue> {
        self.inner.lock().values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Every `(session, edge)` the queue published, in order.
    type SeenEdges = Arc<Mutex<Vec<(String, PendingEdge)>>>;

    fn id(s: &str) -> AskId {
        s.parse().expect("ask id")
    }

    fn raised(call_id: &str) -> PendingEdge {
        PendingEdge::Raised {
            ask_id: id(call_id),
            kind: AskKind::Approval,
            tool: "Bash".into(),
        }
    }

    fn watched_queue() -> (AskQueue, SeenEdges) {
        let queue = AskQueue::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        queue.add_pending_watcher(Arc::new(move |session_id: SessionId, edge| {
            sink.lock().push((session_id.to_string(), edge));
        }));
        (queue, seen)
    }

    fn mirror_req(call_id: &str, session_id: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool_call_id: String::new(),
            call_id: call_id.into(),
            session_id: session_id.into(),
            user_id: "u".into(),
            tool: "Bash".into(),
            accesses: vec![],
            params_preview: String::new(),
            description: None,
        }
    }

    fn ask(call_id: &str, session_id: &str, phase: AskPhase) -> PendingAsk {
        PendingAsk {
            ask_id: id(call_id),
            session_id: session_id.into(),
            tool_call_id: format!("use-{call_id}"),
            kind: AskKind::Approval,
            phase,
            request: AskRequest::Approval(mirror_req(call_id, session_id)),
        }
    }

    #[test]
    fn resolve_head_on_empty_is_noop() {
        let queue = AskQueue::new();
        assert!(!queue.resolve_head(ApprovalDecision::Approve));
    }

    #[test]
    fn pending_edges_fire_once_per_session_boundary_not_once_per_prompt() {
        let (queue, seen) = watched_queue();
        queue.enqueue_mirror(mirror_req("a1", "s1"));
        queue.enqueue_mirror(mirror_req("a2", "s1"));
        queue.enqueue_mirror(mirror_req("b1", "s2"));
        assert_eq!(
            *seen.lock(),
            vec![("s1".into(), raised("a1")), ("s2".into(), raised("b1"))],
            "the second prompt on an already-waiting session is silent"
        );

        seen.lock().clear();
        assert!(queue.drop_call("a1"));
        assert!(seen.lock().is_empty(), "s1 still has a parked prompt");

        assert!(queue.drop_call("a2"));
        assert_eq!(*seen.lock(), vec![("s1".into(), PendingEdge::Abandoned)]);
    }

    #[test]
    fn pending_sessions_dedupes_and_tracks_removal() {
        let (queue, _) = watched_queue();
        queue.enqueue_mirror(mirror_req("a1", "s1"));
        queue.enqueue_mirror(mirror_req("a2", "s1"));
        queue.enqueue_mirror(mirror_req("b1", "s2"));
        assert_eq!(
            queue.pending_sessions(),
            HashSet::from([SessionId::from("s1"), SessionId::from("s2")])
        );
        queue.drop_call("b1");
        assert_eq!(
            queue.pending_sessions(),
            HashSet::from([SessionId::from("s1")])
        );
    }

    #[tokio::test]
    async fn resolving_a_live_entry_delivers_and_publishes_answered() {
        let (queue, seen) = watched_queue();
        let (tx, rx) = oneshot::channel();
        queue.push_live(ask("c1", "s1", AskPhase::BeforeExecute), tx);
        let removed = queue
            .resolve(
                &id("c1"),
                AskResolution::Approval(ApprovalDecision::Approve),
            )
            .expect("queued");
        assert!(removed.delivered);
        assert_eq!(
            rx.await.expect("delivered"),
            AskResolution::Approval(ApprovalDecision::Approve)
        );
        assert_eq!(
            *seen.lock(),
            vec![
                ("s1".into(), raised("c1")),
                ("s1".into(), PendingEdge::Answered)
            ],
            "a human decision is ANSWERED — the next prompt may interrupt again"
        );
    }

    #[test]
    fn abandon_clears_the_session_mark() {
        let (queue, seen) = watched_queue();
        queue.push_dead(ask("c1", "s1", AskPhase::BeforeExecute));
        assert!(queue.abandon(&id("c1")).is_some());
        assert_eq!(
            *seen.lock(),
            vec![
                ("s1".into(), raised("c1")),
                ("s1".into(), PendingEdge::Abandoned)
            ],
            "an ask nobody answered is ABANDONED, not answered"
        );
        assert!(queue.pending_sessions().is_empty());
    }

    #[test]
    fn orphan_keeps_a_resumable_entry_dead_and_silent() {
        let (queue, seen) = watched_queue();
        let (tx, _rx) = oneshot::channel();
        queue.push_live(ask("c1", "s1", AskPhase::BeforeExecute), tx);
        assert_eq!(queue.liveness(&id("c1")), Some(true));
        assert!(queue.has_live(&SessionId::from("s1")));
        queue.orphan(&id("c1"));
        assert_eq!(queue.liveness(&id("c1")), Some(false), "dead, still queued");
        assert!(!queue.has_live(&SessionId::from("s1")));
        assert_eq!(*seen.lock(), vec![("s1".into(), raised("c1"))], "no edge");
    }

    #[test]
    fn orphan_drops_a_non_resumable_entry_with_an_abandoned_edge() {
        let (queue, seen) = watched_queue();
        let (tx, _rx) = oneshot::channel();
        queue.push_live(ask("c1", "s1", AskPhase::DuringExecute), tx);
        queue.orphan(&id("c1"));
        assert!(queue.is_empty());
        assert_eq!(
            *seen.lock(),
            vec![
                ("s1".into(), raised("c1")),
                ("s1".into(), PendingEdge::Abandoned)
            ]
        );
    }

    #[test]
    fn ask_watchers_see_every_raise_and_close_with_the_session() {
        let queue = AskQueue::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        queue.add_ask_watcher(Arc::new(move |event| sink.lock().push(event)));
        queue.push_dead(ask("a", "s1", AskPhase::BeforeExecute));
        queue.push_dead(ask("b", "s1", AskPhase::BeforeExecute));
        queue.abandon(&id("a"));
        let events = seen.lock().clone();
        assert_eq!(events.len(), 3, "per ask, not per session boundary");
        assert_eq!(
            events[2],
            AskQueueEvent::AskClosed {
                session_id: "s1".into(),
                ask_id: id("a"),
                kind: AskKind::Approval,
                outcome: AskResolution::Abandoned,
            }
        );
    }

    #[test]
    fn resolve_head_on_a_mirror_runs_the_resolver() {
        let queue = AskQueue::new();
        let got = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&got);
        queue.set_resolver(Arc::new(move |ask_id, resolution| {
            *sink.lock() = Some((ask_id, resolution));
        }));
        queue.enqueue_mirror(mirror_req("m1", "s1"));
        assert_eq!(
            queue.peek_head_approval().map(|r| r.call_id),
            Some("m1".into())
        );
        assert!(queue.resolve_head(ApprovalDecision::Deny));
        assert_eq!(
            *got.lock(),
            Some((id("m1"), AskResolution::Approval(ApprovalDecision::Deny)))
        );
    }

    #[test]
    fn a_watcher_may_read_the_queue_without_deadlocking() {
        let queue = AskQueue::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (probe, sink) = (queue.clone(), Arc::clone(&seen));
        queue.add_pending_watcher(Arc::new(move |session_id: SessionId, _edge| {
            sink.lock()
                .push((session_id, probe.pending_sessions().len(), probe.len()));
        }));

        queue.enqueue_mirror(mirror_req("a1", "s1"));
        assert_eq!(*seen.lock(), vec![(SessionId::from("s1"), 1, 1)]);
        assert!(queue.drop_call("a1"));
        assert_eq!(seen.lock().len(), 2);
        assert_eq!(seen.lock()[1], (SessionId::from("s1"), 0, 0));
    }

    #[test]
    fn concurrent_raises_and_removals_leave_the_flag_agreeing_with_the_queue() {
        // Quiescence, not ordering: `a_pile_up_during_one_drain_...` pins
        // order.
        for round in 0..200 {
            let queue = AskQueue::new();
            let log = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&log);
            queue.add_pending_watcher(Arc::new(move |_sid, edge: PendingEdge| {
                sink.lock().push(edge.is_pending());
            }));

            let threads: Vec<_> = ["x", "y"]
                .into_iter()
                .map(|call_id| {
                    let q = queue.clone();
                    std::thread::spawn(move || {
                        q.enqueue_mirror(mirror_req(call_id, "s1"));
                        q.drop_call(call_id);
                    })
                })
                .collect();
            for t in threads {
                t.join().expect("worker");
            }

            let published = log.lock().clone();
            assert!(
                !published.is_empty(),
                "round {round}: at least one edge crossed"
            );
            assert_eq!(
                *published.last().expect("an edge"),
                !queue.pending_sessions().is_empty(),
                "round {round}: last published edge disagrees with the queue \
                 (published {published:?}) — the flag is stranded inverted"
            );
            for pair in published.windows(2) {
                assert_ne!(
                    pair[0], pair[1],
                    "round {round}: repeated edge in {published:?}"
                );
            }
            assert!(queue.is_empty(), "round {round}: both prompts were removed");
        }
    }

    #[test]
    fn a_pile_up_during_one_drain_publishes_in_mutation_order() {
        use std::sync::atomic::AtomicUsize;

        let queue = AskQueue::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let parked = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let (log, parked, release, calls) = (
                Arc::clone(&log),
                Arc::clone(&parked),
                Arc::clone(&release),
                Arc::clone(&calls),
            );
            queue.add_pending_watcher(Arc::new(move |_sid, edge: PendingEdge| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    parked.store(true, Ordering::SeqCst);
                    spin_until(&release);
                }
                log.lock().push(edge);
            }));
        }

        let raiser = {
            let q = queue.clone();
            std::thread::spawn(move || q.enqueue_mirror(mirror_req("x", "s1")))
        };
        spin_until(&parked);

        assert!(queue.drop_call("x"));
        queue.enqueue_mirror(mirror_req("y", "s1"));
        release.store(true, Ordering::SeqCst);
        raiser.join().expect("raiser");

        let published = log.lock().clone();
        assert_eq!(published.len(), 3, "got {published:?}");
        assert!(
            matches!(published[0], PendingEdge::Raised { .. }),
            "got {published:?}"
        );
        assert_eq!(published[1], PendingEdge::Abandoned, "got {published:?}");
        assert!(
            matches!(published[2], PendingEdge::Raised { .. }),
            "got {published:?}"
        );
    }

    #[test]
    fn a_panicking_watcher_does_not_wedge_publication() {
        let queue = AskQueue::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let armed = Arc::new(AtomicBool::new(true));
        {
            let (log, armed) = (Arc::clone(&log), Arc::clone(&armed));
            queue.add_pending_watcher(Arc::new(move |_sid, edge: PendingEdge| {
                if armed.swap(false, Ordering::SeqCst) {
                    panic!("watcher blew up mid-drain");
                }
                log.lock().push(edge);
            }));
        }

        let raised = {
            let q = queue.clone();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                q.enqueue_mirror(mirror_req("x", "s1"))
            }))
        };
        assert!(raised.is_err(), "the watcher was supposed to panic");
        assert!(log.lock().is_empty(), "…and to publish nothing");

        assert!(queue.drop_call("x"));
        assert_eq!(
            *log.lock(),
            vec![PendingEdge::Abandoned],
            "publication is wedged after a panicking watcher"
        );
    }

    /// Spin until `flag`, with a deadline so a regression fails the test
    /// instead of hanging the suite.
    fn spin_until(flag: &AtomicBool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !flag.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the drain to reach the watcher"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_queue_with_no_watcher_installed_still_mutates() {
        let queue = AskQueue::new();
        queue.enqueue_mirror(mirror_req("a1", "s1"));
        assert_eq!(queue.len(), 1);
        assert!(queue.drop_call("a1"));
        assert!(queue.is_empty());
    }

    #[test]
    fn queues_hand_out_one_queue_per_channel() {
        let queues = AskQueues::new();
        assert!(queues.get(&ChannelType::owner()).is_none());
        let a = queues.get_or_create(&ChannelType::owner());
        a.enqueue_mirror(mirror_req("a1", "s1"));
        let b = queues.get_or_create(&ChannelType::owner());
        assert_eq!(b.len(), 1, "same queue, not a fresh one");
        assert!(queues.get_or_create(&ChannelType::tui()).is_empty());
    }
}
