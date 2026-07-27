//! User-approval gate for tool execution.
//!
//! A tool call flows through the gate before execution when any
//! [`ResourceAccess`] it declares is not already covered by an
//! [`ApprovedResource`] cached on the session. The gate returns an
//! [`ApprovalDecision`]; `ApproveAlways` instructs the caller to persist the
//! call's resources into session state for the rest of the session.
//!
//! Pure value types ([`ResourceAccess`], [`ApprovedResource`],
//! [`HostPattern`]) live in `baybo-model` so session state can persist them
//! without a cycle back through this crate.
//!
//! Implementations must be `Send + Sync` and safe to call concurrently — the
//! same gate is shared across tool calls that may run in parallel within a
//! single agent turn.
//!
//! ## Reusable channel gate
//!
//! [`ChannelApprovalGate`] + [`ApprovalQueue`] extract the common
//! queue-and-oneshot pattern so each channel only provides a sync waker
//! callback instead of reimplementing the full gate.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use baybo_model::{ChannelType, SessionId};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

pub use baybo_model::approval::{ApprovalDecision, ApprovedResource, HostPattern, ResourceAccess};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A pending approval request forwarded to the gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub call_id: String,
    /// `call_id` of the tool call this prompt blocks — the LLM `tool_use_id`
    /// that `ToolStarted`/`ToolCompleted` events carry, NOT this request's own
    /// `call_id` (a fresh UUID per prompt; one call can prompt more than once).
    /// Lets clients badge the exact work step that is waiting. `None` for a
    /// prompt raised outside a tool-call context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Session the tool call runs under. Lets HTTP clients (e.g. the
    /// gateway-backed TUI) render approvals alongside the session they belong to.
    pub session_id: SessionId,
    /// Baybo user id the tool call is running on behalf of. Sidecars
    /// that route prompts by platform user (Telegram chat, Discord DM)
    /// use this instead of reverse-mapping from `session_id`. Empty
    /// string when no user context applies.
    #[serde(default)]
    pub user_id: String,
    pub tool: String,
    pub accesses: Vec<ResourceAccess>,
    /// Short truncated JSON preview of the parameters, for UI display only.
    pub params_preview: String,
    /// Caller-supplied label produced by [`crate::Tool::call_label`]
    /// (e.g. Bash's `description` parameter). Channel-side approval
    /// UIs can render this above the JSON preview. Wire-compatible
    /// with older sidecars: missing on input deserializes to `None`,
    /// `None` is omitted on serialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Implemented by channel-side UIs (or an auto-deny fallback) to resolve
/// approval requests. Must be safe to call concurrently — a single agent turn
/// may dispatch multiple tool calls in parallel, each going through this gate
/// independently.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision;
}

// ---------------------------------------------------------------------------
// AutoDenyGate
// ---------------------------------------------------------------------------

/// Fallback gate for channels without an approval UX. Denies every request.
/// Fail-closed is intentional: a silent auto-approve would defeat the point.
pub struct AutoDenyGate;

#[async_trait]
impl ApprovalGate for AutoDenyGate {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Deny
    }
}

// ---------------------------------------------------------------------------
// ApprovalGateMap — per-channel gate resolution
// ---------------------------------------------------------------------------

/// Sync-accessible map of approval gates, shared between
/// `ChannelRegistry` (populates at registration time) and `ToolExecutor`
/// (reads at execution time).
///
/// Two tiers, matching the sidecar vs session-scoped client split in the
/// registry:
///
/// * **Type-level** gates, keyed by `ChannelType`. Populated by sidecar
///   registrations (e.g. one Telegram sidecar serving every session).
/// * **Session-level** gates, keyed by `(ChannelType, SessionId)`.
///   Populated by session-scoped client registrations (e.g. per-TUI
///   gates so each TUI only prompts its own user).
///
/// [`get`](Self::get) tries the session-level entry first and falls
/// back to the type-level gate; when neither is present it returns
/// `AutoDenyGate` (fail-closed).
pub struct ApprovalGateMap {
    type_level: DashMap<ChannelType, Arc<dyn ApprovalGate>>,
    session_level: DashMap<(ChannelType, SessionId), Arc<dyn ApprovalGate>>,
}

impl ApprovalGateMap {
    pub fn new() -> Self {
        Self {
            type_level: DashMap::new(),
            session_level: DashMap::new(),
        }
    }

    /// Register a type-level gate for a channel. Used for sidecar
    /// registrations.
    pub fn insert(&self, channel: ChannelType, gate: Arc<dyn ApprovalGate>) {
        self.type_level.insert(channel, gate);
    }

    /// Register a session-scoped gate for one `(ChannelType,
    /// SessionId)` pair. Used for session-scoped client registrations
    /// (e.g. a specific TUI instance).
    pub fn insert_session(
        &self,
        channel: ChannelType,
        session_id: SessionId,
        gate: Arc<dyn ApprovalGate>,
    ) {
        self.session_level.insert((channel, session_id), gate);
    }

    /// Evict a type-level gate. Called on sidecar unregister so tool
    /// calls scheduled after the channel disconnects fall back to the
    /// fail-closed `AutoDenyGate` instead of timing out against a dead
    /// transport.
    pub fn remove(&self, channel: &ChannelType) {
        self.type_level.remove(channel);
    }

    /// Evict a session-scoped gate. Called on session-scoped client
    /// unregister. Cheap no-op if the entry is already gone.
    pub fn remove_session(&self, channel: &ChannelType, session_id: &SessionId) {
        self.session_level
            .remove(&(channel.clone(), session_id.clone()));
    }

    /// Resolve the gate for one tool call. Tries the session-scoped
    /// entry first (a specific TUI instance answering for its own
    /// session) and falls back to the type-level gate (a sidecar that
    /// answers for every session). When neither is registered returns
    /// `AutoDenyGate` (fail-closed).
    pub fn get(&self, channel: &ChannelType, session_id: &SessionId) -> Arc<dyn ApprovalGate> {
        if let Some(entry) = self
            .session_level
            .get(&(channel.clone(), session_id.clone()))
        {
            return Arc::clone(entry.value());
        }
        self.type_level
            .get(channel)
            .map(|e| Arc::clone(e.value()))
            .unwrap_or_else(|| Arc::new(AutoDenyGate))
    }
}

impl Default for ApprovalGateMap {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ApprovalQueue — reusable across channels
// ---------------------------------------------------------------------------

/// A single pending approval awaiting a decision via its oneshot
/// responder.
///
/// `responder` is `Option` so the queue can also hold **display-only
/// mirror** entries — entries sourced from an external authority (e.g.
/// the HTTP gateway's approval stream, replayed into the TUI's local
/// queue for rendering). Those entries have no oneshot to fire; the
/// TUI still needs them in the queue so its existing `peek_head` /
/// modal pipeline works unchanged.
struct PendingApproval {
    req: ApprovalRequest,
    responder: Option<oneshot::Sender<ApprovalDecision>>,
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
    /// because watchers run **under the queue lock** and so must never read
    /// the queue back to find out what woke them — `parking_lot` locks are not
    /// reentrant, and that lookup would deadlock the gate.
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
/// 300 s gate timeout and a cancelled turn both exit through
/// [`QueueCleanup::drop`] and broadcast nothing at all — a "waiting for you"
/// mark hung off the resolution event alone would stick forever on exactly
/// the turns nobody answered.
///
/// Runs **while the queue lock is held**, which is what keeps two concurrent
/// mutations from publishing their edges out of order (a `false` overtaking
/// the `true` that superseded it would strand the flag inverted, i.e. silently
/// stop asking the user for a decision the agent is still blocked on). The
/// implementation must therefore only *publish* — never re-enter the queue,
/// never block.
pub type PendingWatcher = Arc<dyn Fn(SessionId, PendingEdge) + Send + Sync>;

/// Thread-safe queue of pending approval prompts. Shared between a
/// [`ChannelApprovalGate`] (producer) and the channel's event loop
/// (consumer). Cloneable — both sides hold a handle.
#[derive(Clone)]
pub struct ApprovalQueue {
    inner: Arc<Mutex<VecDeque<PendingApproval>>>,
    resolver: Arc<Mutex<Option<ResolveFn>>>,
    pending_watchers: Arc<Mutex<Arc<Vec<PendingWatcher>>>>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
            resolver: Arc::new(Mutex::new(None)),
            pending_watchers: Arc::new(Mutex::new(Arc::new(Vec::new()))),
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

    /// Publish `session_id`'s boundary crossing if the caller's mutation
    /// caused one. `q` is the **still-locked** queue with the mutation already
    /// applied, so it is the truth; `was_pending` is what the caller observed
    /// before mutating, and `edge` labels *why* the crossing happened. See
    /// [`PendingWatcher`] for why this fires under the lock.
    fn note_pending_edge(
        &self,
        q: &VecDeque<PendingApproval>,
        session_id: &SessionId,
        was_pending: bool,
        edge: PendingEdge,
    ) {
        let pending = q.iter().any(|e| &e.req.session_id == session_id);
        if pending == was_pending {
            return;
        }
        debug_assert_eq!(
            pending,
            edge.is_pending(),
            "caller labelled the edge in the opposite direction to the queue"
        );
        let watchers = Arc::clone(&*self.pending_watchers.lock());
        for watcher in watchers.iter() {
            watcher(session_id.clone(), edge.clone());
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

    fn push(&self, entry: PendingApproval) {
        let session_id = entry.req.session_id.clone();
        let raised = PendingEdge::Raised {
            call_id: entry.req.call_id.clone(),
            tool: entry.req.tool.clone(),
        };
        let mut q = self.inner.lock();
        let was_pending = q.iter().any(|e| e.req.session_id == session_id);
        q.push_back(entry);
        self.note_pending_edge(&q, &session_id, was_pending, raised);
    }

    /// Snapshot the head request for rendering. Returns `None` when empty.
    pub fn peek_head(&self) -> Option<ApprovalRequest> {
        self.inner.lock().front().map(|e| e.req.clone())
    }

    /// Resolve the head entry with the given decision, firing its
    /// oneshot if one is attached. Returns `true` if an entry was
    /// popped, `false` when the queue was empty. Channels call this
    /// from their keypress handler — the oneshot unblocks the
    /// `ChannelApprovalGate::request` future. Entries with no
    /// responder (remote mirrors) pop silently; the caller handles
    /// the actual resolution out-of-band.
    pub fn resolve_head(&self, decision: ApprovalDecision) -> bool {
        let popped = {
            let mut q = self.inner.lock();
            let popped = q.pop_front();
            if let Some(pending) = popped.as_ref() {
                self.note_pending_edge(&q, &pending.req.session_id, true, PendingEdge::Answered);
            }
            popped
        };
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
        let pending = {
            let mut q = self.inner.lock();
            let pos = q.iter().position(|e| e.req.call_id == call_id)?;
            let pending = q.remove(pos)?;
            self.note_pending_edge(&q, &pending.req.session_id, true, PendingEdge::Answered);
            pending
        };
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
        let mut q = self.inner.lock();
        let Some(pos) = q.iter().position(|e| e.req.call_id == call_id) else {
            return false;
        };
        let Some(removed) = q.remove(pos) else {
            return false;
        };
        self.note_pending_edge(&q, &removed.req.session_id, true, PendingEdge::Abandoned);
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

impl Default for ApprovalQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ChannelApprovalGate — reusable across channels
// ---------------------------------------------------------------------------

/// Generic [`ApprovalGate`] for any channel. Pushes requests onto an
/// [`ApprovalQueue`] and fires a sync waker callback so the channel's event
/// loop redraws. The channel resolves entries by calling
/// [`ApprovalQueue::resolve_head`].
///
/// Fail-closed: if the oneshot is dropped without a decision (e.g. the
/// channel exits while approvals are pending) or the timeout expires,
/// `Deny` is returned.
pub struct ChannelApprovalGate {
    queue: ApprovalQueue,
    /// Handed the request it is being woken FOR. It must not re-read the queue
    /// to find one: the queue is per-CHANNEL, so two sessions prompting at once
    /// race, and a waker that took (say) the tail would broadcast one prompt
    /// twice and the other never — leaving a user staring at a turn that
    /// silently denies itself minutes later.
    waker: Arc<dyn Fn(ApprovalRequest) + Send + Sync>,
    timeout: Duration,
}

impl ChannelApprovalGate {
    pub fn new(
        queue: ApprovalQueue,
        waker: Arc<dyn Fn(ApprovalRequest) + Send + Sync>,
        timeout: Duration,
    ) -> Self {
        Self {
            queue,
            waker,
            timeout,
        }
    }

    /// Access the underlying queue so the channel can peek/resolve entries.
    pub fn queue(&self) -> &ApprovalQueue {
        &self.queue
    }
}

#[async_trait]
impl ApprovalGate for ChannelApprovalGate {
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
        let call_id = req.call_id.clone();
        let (tx, rx) = oneshot::channel();
        let woken_for = req.clone();
        self.queue.push(PendingApproval {
            req,
            responder: Some(tx),
        });
        (self.waker)(woken_for);

        // RAII guard: if our future is dropped before a decision lands
        // (e.g. the outer tool timeout fires while the user is still
        // reading the modal) or the gate's own timeout elapses, remove
        // the queue entry so the channel UI doesn't catch the user's
        // *next* click on a stale, disconnected `tx` — which would
        // cost an extra click per orphan and could let a stale
        // resolution silently land on a disconnected receiver.
        let _cleanup = QueueCleanup {
            queue: self.queue.clone(),
            call_id,
        };

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => decision,
            _ => ApprovalDecision::Deny,
        }
    }
}

/// RAII helper that removes a queue entry when its owning request
/// future is dropped. A no-op if `resolve_head` / `resolve_by_call_id`
/// already popped the entry (the matching `call_id` is gone, and
/// `drop_call` returns `false`).
struct QueueCleanup {
    queue: ApprovalQueue,
    call_id: String,
}

impl Drop for QueueCleanup {
    fn drop(&mut self) {
        let _ = self.queue.drop_call(&self.call_id);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Heuristic JSON preview used by the agent layer when constructing an
/// [`ApprovalRequest`]. Truncates overly long strings so the UI does not have
/// to re-implement this.
pub fn preview_params(params: &serde_json::Value, max_len: usize) -> String {
    let s = serde_json::to_string(params).unwrap_or_else(|_| params.to_string());
    if s.len() <= max_len {
        s
    } else {
        let mut end = max_len;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_deny_gate_always_denies() {
        let gate = AutoDenyGate;
        let out = gate
            .request(ApprovalRequest {
                tool_call_id: None,
                call_id: "x".into(),
                session_id: "s".into(),
                user_id: "u".into(),
                tool: "t".into(),
                accesses: vec![],
                params_preview: String::new(),
                description: None,
            })
            .await;
        assert_eq!(out, ApprovalDecision::Deny);
    }

    #[tokio::test]
    async fn the_waker_is_handed_the_request_it_was_woken_for() {
        // The regression this pins: the waker used to re-read the queue to find
        // "the" pending entry. The queue is per-CHANNEL, so two sessions
        // prompting at once interleave push/wake — and a waker that re-read the
        // tail announced one prompt twice while the other was never announced at
        // all, leaving that turn to silently deny itself on timeout.
        let queue = ApprovalQueue::new();
        let woken: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let woken_for_gate = Arc::clone(&woken);
        let gate = Arc::new(ChannelApprovalGate::new(
            queue.clone(),
            Arc::new(move |req: ApprovalRequest| {
                woken_for_gate.lock().push(req.call_id);
            }),
            Duration::from_millis(50),
        ));

        let req = |call_id: &str| ApprovalRequest {
            tool_call_id: None,
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
        assert_eq!(ra, ApprovalDecision::Deny, "unanswered gate fails closed");
        assert_eq!(rb, ApprovalDecision::Deny);

        let mut seen = woken.lock().clone();
        seen.sort();
        assert_eq!(
            seen,
            vec!["a".to_string(), "b".to_string()],
            "every prompt is announced exactly once, with its own id"
        );
    }

    #[test]
    fn preview_truncates_long_params() {
        let v = serde_json::json!({ "s": "x".repeat(500) });
        let p = preview_params(&v, 64);
        assert!(p.len() <= 67);
        assert!(p.ends_with('…'));
    }

    #[tokio::test]
    async fn channel_gate_resolves_via_queue() {
        let queue = ApprovalQueue::new();
        let woken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let woken2 = Arc::clone(&woken);
        let gate = ChannelApprovalGate::new(
            queue.clone(),
            Arc::new(move |_| {
                woken2.store(true, std::sync::atomic::Ordering::SeqCst);
            }),
            Duration::from_secs(60),
        );

        let req = ApprovalRequest {
            tool_call_id: None,
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
        assert_eq!(handle.await.unwrap(), ApprovalDecision::ApproveAlways);
        assert!(queue.is_empty());
    }

    /// When the oneshot sender is dropped without a decision (e.g. the
    /// channel exits mid-approval), `rx.await` fails and the gate maps
    /// it to `Deny` — fail-closed. We test this via [`oneshot`] directly
    /// since `ApprovalQueue` intentionally hides the responder.
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
        let queue = ApprovalQueue::new();
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
        let queue = ApprovalQueue::new();
        let gate =
            ChannelApprovalGate::new(queue.clone(), Arc::new(|_| {}), Duration::from_secs(60));

        let req = ApprovalRequest {
            tool_call_id: None,
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
        let queue = ApprovalQueue::new();
        let gate =
            ChannelApprovalGate::new(queue.clone(), Arc::new(|_| {}), Duration::from_secs(60));

        let req = ApprovalRequest {
            tool_call_id: None,
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
        assert_eq!(task.await.unwrap(), ApprovalDecision::Approve);
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
    fn watched_queue() -> (ApprovalQueue, SeenEdges) {
        let queue = ApprovalQueue::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        queue.add_pending_watcher(Arc::new(move |session_id: SessionId, edge| {
            sink.lock().push((session_id.to_string(), edge));
        }));
        (queue, seen)
    }

    fn mirror_req(call_id: &str, session_id: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool_call_id: None,
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
        let gate =
            ChannelApprovalGate::new(queue.clone(), Arc::new(|_| {}), Duration::from_millis(30));
        assert_eq!(
            gate.request(mirror_req("c1", "s1")).await,
            ApprovalDecision::Deny,
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
        let gate = Arc::new(ChannelApprovalGate::new(
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
        let gate =
            ChannelApprovalGate::new(queue.clone(), Arc::new(|_| {}), Duration::from_secs(60));
        let q = queue.clone();
        let task = tokio::spawn(async move { gate.request(mirror_req("c1", "s1")).await });
        tokio::task::yield_now().await;
        assert_eq!(
            q.resolve_by_call_id("c1", ApprovalDecision::Approve),
            Some(SessionId::from("s1"))
        );
        assert_eq!(task.await.unwrap(), ApprovalDecision::Approve);
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
    fn a_queue_with_no_watcher_installed_still_mutates() {
        // The TUI's client-side mirror queue installs no watcher.
        let queue = ApprovalQueue::new();
        queue.enqueue_mirror(mirror_req("a1", "s1"));
        assert_eq!(queue.len(), 1);
        assert!(queue.drop_call("a1"));
        assert!(queue.is_empty());
    }
}
