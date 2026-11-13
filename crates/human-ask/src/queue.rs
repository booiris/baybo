//! The per-channel queue of parked approval prompts.
//!
//! [`crate::ChannelAskGate`] + [`AskQueue`] extract the common
//! queue-and-oneshot pattern so each channel only provides a sync waker
//! callback instead of reimplementing the full gate.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use baybo_model::SessionId;
use baybo_model::approval::{ApprovalDecision, ApprovalRequest};
use parking_lot::Mutex;
use tokio::sync::oneshot;

/// A single pending approval awaiting a decision via its oneshot
/// responder.
///
/// `responder` is `Option` so the queue can also hold **display-only
/// mirror** entries — entries sourced from an external authority (e.g.
/// the HTTP gateway's approval stream, replayed into the TUI's local
/// queue for rendering). Those entries have no oneshot to fire; the
/// TUI still needs them in the queue so its existing `peek_head` /
/// modal pipeline works unchanged.
pub(crate) struct PendingApproval {
    pub(crate) req: ApprovalRequest,
    pub(crate) responder: Option<oneshot::Sender<ApprovalDecision>>,
}

/// Callback fired when a mirror entry is resolved locally, carrying the
/// popped `(call_id, decision)`. The TUI transport installs one so the
/// local resolution also POSTs to the gateway's authoritative gate. The
/// callback runs synchronously from the resolve path; implementations
/// that need async work should spawn a task themselves.
pub type ResolveFn = Arc<dyn Fn(String, ApprovalDecision) + Send + Sync>;

/// Which way a session crossed the "has at least one parked prompt" boundary,
/// and — on the way out — whether a human was behind it.
///
/// The distinction is not cosmetic. [`Self::Answered`] means the user is
/// present and acting, so the next prompt deserves to interrupt them again
/// immediately; [`Self::Abandoned`] means nobody came, and re-announcing the
/// same conversation at the same volume is how an agent that keeps retrying
/// gated approaches turns into an all-night stream of notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingEdge {
    /// The session's FIRST prompt just parked. Carries the prompt's identity
    /// so a watcher never has to read the queue back to find out what woke it
    /// — a lookup that is now merely wasteful, and used to be a deadlock.
    Raised { call_id: String, tool: String },
    /// Its LAST prompt was answered by a decision someone made.
    Answered,
    /// Its LAST prompt went away with no decision — the gate timed out, or
    /// the turn was cancelled out from under it.
    Abandoned,
}

impl PendingEdge {
    /// Whether the session is waiting on the user after this edge.
    pub fn is_pending(&self) -> bool {
        matches!(self, PendingEdge::Raised { .. })
    }
}

/// Fired when a session crosses the "has at least one parked prompt"
/// boundary. **Edge-only**: a second prompt on an already-waiting session is
/// silent, and resolving one of two leaves the session waiting.
///
/// This exists because the queue is the *only* place that sees every way a
/// prompt leaves. A client resolve broadcasts `ApprovalResolved`, but the
/// 300 s gate timeout and a cancelled turn both exit through the
/// `QueueCleanup` guard in [`crate::ChannelAskGate`] and broadcast nothing
/// at all — a "waiting for you" mark hung off the resolution event alone
/// would stick forever on exactly the turns nobody answered.
///
/// Runs with **no queue lock held**, so a watcher may read the queue back.
/// It still runs INLINE, on whichever thread wins the drain — usually the
/// blocked tool's own future — and that thread keeps draining while others
/// enqueue, so it can be conscripted into publishing *other* sessions' edges.
/// A slow watcher therefore still delays a turn; it just no longer holds the
/// gate's lock while doing it. Keep it non-blocking.
///
/// Order is preserved: edges are appended to `outbox` under the queue lock, so
/// their order is the order the mutations happened, and
/// [`AskQueue::publish_pending`] lets only one thread drain at a time.
/// That ordering is load-bearing — a stale `false` overtaking the `true` that
/// superseded it would strand the flag inverted, silently no longer asking the
/// user for a decision the agent is still blocked on.
///
/// A watcher that MUTATES the queue re-enters `publish_pending`, which returns
/// immediately (the active publisher drains what the mutation enqueued), so
/// that is safe too — but it makes the watcher's own edge land after the one it
/// is handling, which is rarely what such code wants.
pub type PendingWatcher = Arc<dyn Fn(SessionId, PendingEdge) + Send + Sync>;

/// Thread-safe queue of pending approval prompts. Shared between a
/// [`ChannelAskGate`](crate::ChannelAskGate) (producer) and the channel's event loop
/// (consumer). Cloneable — both sides hold a handle.
#[derive(Clone)]
pub struct AskQueue {
    inner: Arc<Mutex<VecDeque<PendingApproval>>>,
    resolver: Arc<Mutex<Option<ResolveFn>>>,
    pending_watchers: Arc<Mutex<Arc<Vec<PendingWatcher>>>>,
    /// Edges awaiting publication, in the order their mutations happened.
    ///
    /// The indirection exists so foreign watcher code never runs under the
    /// queue lock. Appending here is the only thing a mutation does while
    /// holding `inner`, and appending is what fixes the order — the draining
    /// happens afterwards, outside the lock.
    outbox: Arc<Mutex<VecDeque<(SessionId, PendingEdge)>>>,
    /// Whether a thread is currently draining [`Self::outbox`]. At most one
    /// may, which is what keeps two concurrent drains from interleaving their
    /// callbacks and inverting the order the outbox established.
    publishing: Arc<AtomicBool>,
}

impl AskQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
            resolver: Arc::new(Mutex::new(None)),
            pending_watchers: Arc::new(Mutex::new(Arc::new(Vec::new()))),
            outbox: Arc::new(Mutex::new(VecDeque::new())),
            publishing: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Add a [`PendingWatcher`] fired on every per-session
    /// has-parked-prompts edge. **Appends** — the chat-list broadcast and the
    /// push relay are installed at different moments in boot (one when the
    /// channel is built, one when the push dispatcher exists), and a
    /// replace-style slot would silently leave whichever lost the race with
    /// no signal at all. Watchers fire in install order, each independently.
    pub fn add_pending_watcher(&self, watcher: PendingWatcher) {
        let mut slot = self.pending_watchers.lock();
        let mut next = Vec::clone(slot.as_ref());
        next.push(watcher);
        *slot = Arc::new(next);
    }

    /// Queue `session_id`'s boundary crossing for publication if the caller's
    /// mutation caused one. `q` is the **still-locked** queue with the mutation
    /// already applied, so it is the truth; `was_pending` is what the caller
    /// observed before mutating, and `edge` labels *why* the crossing happened.
    ///
    /// Enqueues only, and returns whether it did. A caller that enqueued must
    /// follow the lock release with [`Self::publish_pending`]; one that did not
    /// must NOT — a mutation that crosses no boundary (a second prompt on an
    /// already-waiting session, a `resolve_head` on an empty queue) ran no
    /// foreign code before this indirection existed, and should not start now.
    fn note_pending_edge(
        &self,
        q: &VecDeque<PendingApproval>,
        session_id: &SessionId,
        was_pending: bool,
        edge: PendingEdge,
    ) -> bool {
        let pending = q.iter().any(|e| &e.req.session_id == session_id);
        if pending == was_pending {
            return false;
        }
        debug_assert_eq!(
            pending,
            edge.is_pending(),
            "caller labelled the edge in the opposite direction to the queue"
        );
        self.outbox.lock().push_back((session_id.clone(), edge));
        true
    }

    /// Drain [`Self::outbox`] into the watchers. **Call only with the queue
    /// lock released** — that is the whole point of the outbox.
    ///
    /// Non-blocking: a second caller arriving while another thread is draining
    /// returns immediately, because that thread will pick up whatever the
    /// second caller enqueued. Single-drainer is also what preserves order.
    fn publish_pending(&self) {
        /// Releases the drain claim on the unwind path too. A watcher that
        /// panics must merely DELAY the remaining edges to the next mutation —
        /// a plain store on the fall-through path would be skipped by the
        /// unwind and bounce every future publisher forever, silently ending
        /// all chat-list marks and approval pushes for the process. (Release
        /// builds set `panic = "abort"`, so this is a dev/test wedge, not a
        /// production one — but `PendingWatcher` is a public seam.)
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
                    // Scoped so the outbox lock is NOT held across a callback.
                    let next = self.outbox.lock().pop_front();
                    let Some((session_id, edge)) = next else {
                        break;
                    };
                    let watchers = Arc::clone(&*self.pending_watchers.lock());
                    for watcher in watchers.iter() {
                        watcher(session_id.clone(), edge.clone());
                    }
                }
            }
            // Something may have been enqueued between the last pop and the
            // claim's release. Re-claim and drain it, unless another thread
            // already claimed the slot — in which case it will.
            if self.outbox.lock().is_empty() || self.publishing.swap(true, Ordering::AcqRel) {
                return;
            }
        }
    }

    /// Sessions with at least one parked prompt. One pass cloning only the
    /// ids — the chat-list endpoint asks once per refresh and must not pay
    /// [`Self::list`]'s clone of every request body to answer a bool.
    pub fn pending_sessions(&self) -> HashSet<SessionId> {
        self.inner
            .lock()
            .iter()
            .map(|e| e.req.session_id.clone())
            .collect()
    }

    /// Install a callback that runs whenever a queue entry is resolved
    /// locally. The TUI transport uses this to forward the user's
    /// decision back to the gateway's `/v1/approvals/:id` endpoint.
    /// Replaces any previously-installed resolver.
    pub fn set_resolver(&self, resolver: ResolveFn) {
        *self.resolver.lock() = Some(resolver);
    }

    fn resolver(&self) -> Option<ResolveFn> {
        self.resolver.lock().clone()
    }

    pub(crate) fn push(&self, entry: PendingApproval) {
        let session_id = entry.req.session_id.clone();
        let raised = PendingEdge::Raised {
            call_id: entry.req.call_id.clone(),
            tool: entry.req.tool.clone(),
        };
        let crossed = {
            let mut q = self.inner.lock();
            let was_pending = q.iter().any(|e| e.req.session_id == session_id);
            q.push_back(entry);
            self.note_pending_edge(&q, &session_id, was_pending, raised)
        };
        if crossed {
            self.publish_pending();
        }
    }

    /// Snapshot the head request for rendering. Returns `None` when empty.
    pub fn peek_head(&self) -> Option<ApprovalRequest> {
        self.inner.lock().front().map(|e| e.req.clone())
    }

    /// Resolve the head entry with the given decision, firing its
    /// oneshot if one is attached. Returns `true` if an entry was
    /// popped, `false` when the queue was empty. Channels call this
    /// from their keypress handler — the oneshot unblocks the
    /// `ChannelAskGate::request` future. Entries with no
    /// responder (remote mirrors) pop silently; the caller handles
    /// the actual resolution out-of-band.
    pub fn resolve_head(&self, decision: ApprovalDecision) -> bool {
        let (popped, crossed) = {
            let mut q = self.inner.lock();
            let popped = q.pop_front();
            let crossed = popped.as_ref().is_some_and(|pending| {
                self.note_pending_edge(&q, &pending.req.session_id, true, PendingEdge::Answered)
            });
            (popped, crossed)
        };
        if crossed {
            self.publish_pending();
        }
        let Some(pending) = popped else {
            return false;
        };
        match pending.responder {
            Some(responder) => {
                let _ = responder.send(decision);
            }
            None => {
                if let Some(resolver) = self.resolver() {
                    resolver(pending.req.call_id.clone(), decision);
                }
            }
        }
        true
    }

    /// Resolve a pending approval by its `call_id`. Used by REST
    /// clients (e.g. the HTTP gateway) where FIFO ordering on the
    /// wire is not guaranteed and the UI may resolve approvals out
    /// of submission order. Returns the resolved entry's
    /// `session_id` on a hit — callers stamp it onto the follow-up
    /// `dispatch_approval_resolved` so the fan-out reaches the
    /// session's other subscribers instead of dispatching with an
    /// empty id (which on a `ChannelKind::Subscribed` channel would
    /// silently match no subscribers).
    pub fn resolve_by_call_id(
        &self,
        call_id: &str,
        decision: ApprovalDecision,
    ) -> Option<SessionId> {
        let (pending, crossed) = {
            let mut q = self.inner.lock();
            let pos = q.iter().position(|e| e.req.call_id == call_id)?;
            let pending = q.remove(pos)?;
            let crossed =
                self.note_pending_edge(&q, &pending.req.session_id, true, PendingEdge::Answered);
            (pending, crossed)
        };
        if crossed {
            self.publish_pending();
        }
        let session_id = pending.req.session_id.clone();
        // Outside the lock: the oneshot wakes the blocked tool future, which
        // may run to its next `gate.request` on another worker and re-enter
        // the queue.
        if let Some(responder) = pending.responder {
            let _ = responder.send(decision);
        }
        Some(session_id)
    }

    /// Append a display-only mirror entry (no oneshot). Used by the
    /// TUI transport to reflect approvals queued on the gateway into
    /// the local queue so the existing TUI approval modal picks them
    /// up. Resolution is still driven by the gateway — the local
    /// `resolve_head` / `resolve_by_call_id` call just pops the
    /// mirror entry.
    pub fn enqueue_mirror(&self, req: ApprovalRequest) {
        self.push(PendingApproval {
            req,
            responder: None,
        });
    }

    /// Remove the entry with the given `call_id` without firing any
    /// responder. Used by the TUI when the gateway broadcasts that
    /// another client resolved the same approval, so we drop the
    /// local mirror without second-guessing the decision.
    pub fn drop_call(&self, call_id: &str) -> bool {
        let crossed;
        {
            let mut q = self.inner.lock();
            let Some(pos) = q.iter().position(|e| e.req.call_id == call_id) else {
                return false;
            };
            let Some(removed) = q.remove(pos) else {
                return false;
            };
            crossed =
                self.note_pending_edge(&q, &removed.req.session_id, true, PendingEdge::Abandoned);
        }
        if crossed {
            self.publish_pending();
        }
        true
    }

    /// Snapshot the full queue for listing via a REST endpoint. Order
    /// matches insertion order.
    pub fn list(&self) -> Vec<ApprovalRequest> {
        self.inner.lock().iter().map(|e| e.req.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for AskQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChannelAskGate;
    use baybo_model::approval::ResourceAccess;
    use baybo_tools::{ApprovalGate, ApprovalOutcome};
    use std::time::Duration;

    #[tokio::test]
    async fn the_waker_is_handed_the_request_it_was_woken_for() {
        // The regression this pins: the waker used to re-read the queue to find
        // "the" pending entry. The queue is per-CHANNEL, so two sessions
        // prompting at once interleave push/wake — and a waker that re-read the
        // tail announced one prompt twice while the other was never announced at
        // all, leaving that turn to silently deny itself on timeout.
        let queue = AskQueue::new();
        let woken: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let woken_for_gate = Arc::clone(&woken);
        let gate = Arc::new(ChannelAskGate::new(
            queue.clone(),
            Arc::new(move |req: ApprovalRequest| {
                woken_for_gate.lock().push(req.call_id);
            }),
            Duration::from_millis(50),
        ));

        let req = |call_id: &str| ApprovalRequest {
            tool_call_id: String::new(),
            call_id: call_id.into(),
            session_id: "s".into(),
            user_id: "u".into(),
            tool: "Bash".into(),
            accesses: vec![ResourceAccess::ExecCommand {
                command: "x".into(),
            }],
            params_preview: "{}".into(),
            description: None,
        };

        // Two prompts in flight on the one shared queue; both time out (nobody
        // answers), which is irrelevant — what matters is what the waker saw.
        let a = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.request(req("a")).await }
        });
        let b = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.request(req("b")).await }
        });
        let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
        assert_eq!(
            ra,
            ApprovalOutcome::timed_out(),
            "unanswered gate fails closed"
        );
        assert_eq!(rb, ApprovalOutcome::timed_out());

        let mut seen = woken.lock().clone();
        seen.sort();
        assert_eq!(
            seen,
            vec!["a".to_string(), "b".to_string()],
            "every prompt is announced exactly once, with its own id"
        );
    }

    #[tokio::test]
    async fn channel_gate_resolves_via_queue() {
        let queue = AskQueue::new();
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let woken2 = Arc::clone(&woken);
        let gate = ChannelAskGate::new(
            queue.clone(),
            Arc::new(move |_| {
                woken2.store(true, std::sync::atomic::Ordering::SeqCst);
            }),
            Duration::from_secs(60),
        );

        let req = ApprovalRequest {
            tool_call_id: String::new(),
            call_id: "c1".into(),
            session_id: "s".into(),
            user_id: "u".into(),
            tool: "read".into(),
            accesses: vec![],
            params_preview: String::new(),
            description: None,
        };
        let handle = tokio::spawn(async move { gate.request(req).await });

        // Spin until the waker fires and the queue is non-empty.
        tokio::task::yield_now().await;
        assert!(woken.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!queue.is_empty());

        // Resolve from the consumer side.
        assert!(queue.resolve_head(ApprovalDecision::ApproveAlways));
        assert_eq!(
            handle.await.unwrap(),
            ApprovalOutcome::answered(ApprovalDecision::ApproveAlways)
        );
        assert!(queue.is_empty());
    }

    /// When the oneshot sender is dropped without a decision (e.g. the
    /// channel exits mid-approval), `rx.await` fails and the gate maps
    /// it to `Deny` — fail-closed. We test this via [`oneshot`] directly
    /// since `AskQueue` intentionally hides the responder.
    #[tokio::test]
    async fn dropped_responder_yields_deny() {
        let (tx, rx) = oneshot::channel::<ApprovalDecision>();
        drop(tx);
        assert_eq!(
            rx.await.unwrap_or(ApprovalDecision::Deny),
            ApprovalDecision::Deny
        );
    }

    #[test]
    fn resolve_head_on_empty_is_noop() {
        let queue = AskQueue::new();
        assert!(!queue.resolve_head(ApprovalDecision::Approve));
    }

    /// Regression for codex adversarial review #3 (medium): when a
    /// caller drops the request future before resolution (typical
    /// scenario: an outer tool-execution timeout cancels the future
    /// while the user is still reading the approval modal), the queue
    /// entry must be removed so a follow-up `resolve_head` does not
    /// pop a now-disconnected entry and cost the user an extra click.
    #[tokio::test]
    async fn dropped_request_future_cleans_up_queue_entry() {
        let queue = AskQueue::new();
        let gate = ChannelAskGate::new(queue.clone(), Arc::new(|_| {}), Duration::from_secs(60));

        let req = ApprovalRequest {
            tool_call_id: String::new(),
            call_id: "stale".into(),
            session_id: "s".into(),
            user_id: "u".into(),
            tool: "t".into(),
            accesses: vec![],
            params_preview: String::new(),
            description: None,
        };

        // Outer wrapper drops the future before any resolution lands —
        // simulates `tokio::time::timeout` firing on the tool side
        // while the user is still reading the modal.
        let res = tokio::time::timeout(Duration::from_millis(20), gate.request(req)).await;
        assert!(res.is_err(), "outer timeout must fire first");

        // Without cleanup, the entry would still be in the queue with
        // a now-disconnected `tx` and the next `resolve_head` would
        // silently waste the user's click on it.
        assert!(
            queue.is_empty(),
            "dropped request must remove its queue entry"
        );
        assert!(
            !queue.resolve_head(ApprovalDecision::Approve),
            "no live entry should remain"
        );
    }

    /// Resolution before drop still works: the entry is popped first
    /// by the consumer, so the cleanup guard's call to `drop_call` is
    /// a harmless no-op (`false`).
    #[tokio::test]
    async fn cleanup_guard_is_noop_when_consumer_resolves_first() {
        let queue = AskQueue::new();
        let gate = ChannelAskGate::new(queue.clone(), Arc::new(|_| {}), Duration::from_secs(60));

        let req = ApprovalRequest {
            tool_call_id: String::new(),
            call_id: "live".into(),
            session_id: "s".into(),
            user_id: "u".into(),
            tool: "t".into(),
            accesses: vec![],
            params_preview: String::new(),
            description: None,
        };
        let q = queue.clone();
        let task = tokio::spawn(async move { gate.request(req).await });
        tokio::task::yield_now().await;
        assert!(q.resolve_head(ApprovalDecision::Approve));
        assert_eq!(
            task.await.unwrap(),
            ApprovalOutcome::answered(ApprovalDecision::Approve)
        );
        assert!(queue.is_empty());
    }

    // --- per-session pending edges (the chat-list "waiting for you" mark) ---

    /// Every `(session, pending)` edge the queue published, in order.
    type SeenEdges = Arc<Mutex<Vec<(String, PendingEdge)>>>;

    /// The edge a first-parked prompt publishes, for assertions.
    fn raised(call_id: &str) -> PendingEdge {
        PendingEdge::Raised {
            call_id: call_id.into(),
            tool: "Bash".into(),
        }
    }

    /// A queue whose pending-edge watcher records into [`SeenEdges`].
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

        // Resolving one of s1's two leaves the mark up — the session is still
        // blocked on the other.
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
    async fn a_timed_out_gate_clears_the_session_mark() {
        // The invariant no resolution event can provide: a gate nobody answers
        // self-denies and broadcasts NOTHING, so the queue's own drop path is
        // the only thing that can retire the chat-list mark. Without it the row
        // wears "waiting for your approval" forever.
        let (queue, seen) = watched_queue();
        let gate = ChannelAskGate::new(queue.clone(), Arc::new(|_| {}), Duration::from_millis(30));
        assert_eq!(
            gate.request(mirror_req("c1", "s1")).await,
            ApprovalOutcome::timed_out(),
            "an unanswered gate fails closed"
        );
        assert_eq!(
            *seen.lock(),
            vec![
                ("s1".into(), raised("c1")),
                ("s1".into(), PendingEdge::Abandoned)
            ],
            "a gate nobody answered is ABANDONED, not answered"
        );
        assert!(queue.pending_sessions().is_empty());
    }

    #[tokio::test]
    async fn a_dropped_request_future_clears_the_session_mark() {
        // `/stop` (or any cancellation) drops the request future mid-wait; the
        // same RAII cleanup must publish the clear.
        let (queue, seen) = watched_queue();
        let gate = Arc::new(ChannelAskGate::new(
            queue.clone(),
            Arc::new(|_| {}),
            Duration::from_secs(60),
        ));
        let task = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.request(mirror_req("c1", "s1")).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(*seen.lock(), vec![("s1".into(), raised("c1"))]);
        task.abort();
        let _ = task.await;
        assert_eq!(
            *seen.lock(),
            vec![
                ("s1".into(), raised("c1")),
                ("s1".into(), PendingEdge::Abandoned)
            ]
        );
    }

    #[tokio::test]
    async fn resolving_by_call_id_clears_the_session_mark() {
        let (queue, seen) = watched_queue();
        let gate = ChannelAskGate::new(queue.clone(), Arc::new(|_| {}), Duration::from_secs(60));
        let q = queue.clone();
        let task = tokio::spawn(async move { gate.request(mirror_req("c1", "s1")).await });
        tokio::task::yield_now().await;
        assert_eq!(
            q.resolve_by_call_id("c1", ApprovalDecision::Approve),
            Some(SessionId::from("s1"))
        );
        assert_eq!(
            task.await.unwrap(),
            ApprovalOutcome::answered(ApprovalDecision::Approve)
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
    fn a_watcher_may_read_the_queue_without_deadlocking() {
        // The capability the outbox buys. Watchers used to run under the queue
        // lock, so this exact call — the obvious thing to write — would wedge
        // the gate forever, and only a doc comment stood between a future
        // watcher and that. Now it just works.
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
        // Quiescence: however two threads interleave, once they are done the
        // last edge published must agree with the queue's final state. A stale
        // `false` landing after the `true` that superseded it reads as "nothing
        // is waiting" while a turn is still blocked on the user — the exact
        // inversion this whole feature exists to prevent.
        //
        // This does NOT prove ordering: with two threads doing two lock-bounded
        // ops each the outbox is essentially never deeper than one entry, so a
        // reversed drain has nothing to invert. `a_pile_up_during_one_drain_...`
        // is the test that pins order; measured against a LIFO mutation, this
        // one catches it ~0% of the time and that one catches it always.
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
            // One session, so the edges are boundary crossings and must
            // alternate; two `true`s in a row would mean an edge was published
            // that never happened.
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
        // The ordering test with teeth. Park a watcher mid-drain so a second
        // thread can pile two more edges behind the one being published, then
        // release it and check what comes out. Reverse the drain and this
        // fails every time.
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

        // The raiser holds the drain claim, so both of these only enqueue.
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
        // The drain claim must be released on the unwind path. A plain store on
        // the fall-through path is skipped by a panicking watcher, and every
        // later publisher then bounces off a claim nobody holds — chat-list
        // marks and approval pushes stop for the life of the process. Release
        // builds abort on panic so this is a dev/test wedge, but the watcher is
        // a public seam and the failure is silent.
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

        // Publication must still work afterwards.
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
        // The TUI's client-side mirror queue installs no watcher.
        let queue = AskQueue::new();
        queue.enqueue_mirror(mirror_req("a1", "s1"));
        assert_eq!(queue.len(), 1);
        assert!(queue.drop_call("a1"));
        assert!(queue.is_empty());
    }
}
