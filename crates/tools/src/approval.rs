//! User-approval gate for tool execution.
//!
//! A tool call flows through the gate before execution when any
//! [`ResourceAccess`] it declares is not already covered by an
//! [`ApprovedResource`] cached on the session. The gate returns an
//! [`ApprovalDecision`]; `ApproveAlways` instructs the caller to persist the
//! call's resources into session state for the rest of the session.
//!
//! Pure value types ([`ResourceAccess`], [`ApprovedResource`],
//! [`HostPattern`]) live in `aura-model` so session state can persist them
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

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_model::ChannelType;
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

pub use aura_model::approval::{ApprovedResource, HostPattern, ResourceAccess};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Decision returned by an [`ApprovalGate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Allow this call only.
    Approve,
    /// Allow this call and remember every resource it touches for the rest of
    /// the session (persisted via `SessionState::approved_resources`).
    ApproveAlways,
    /// Reject the call. The executor surfaces this as `ToolError::Denied`.
    Deny,
}

/// A pending approval request forwarded to the gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub call_id: String,
    /// Session the tool call runs under. Lets HTTP clients (e.g. the
    /// gateway-backed TUI) render approvals alongside the session they belong to.
    pub session_id: String,
    /// Aura user id the tool call is running on behalf of. Sidecars
    /// that route prompts by platform user (Telegram chat, Discord DM)
    /// use this instead of reverse-mapping from `session_id`. Empty
    /// string when no user context applies.
    #[serde(default)]
    pub user_id: String,
    pub tool: String,
    pub accesses: Vec<ResourceAccess>,
    /// Short truncated JSON preview of the parameters, for UI display only.
    pub params_preview: String,
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
    session_level: DashMap<(ChannelType, String), Arc<dyn ApprovalGate>>,
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
        session_id: String,
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
    pub fn remove_session(&self, channel: &ChannelType, session_id: &str) {
        self.session_level
            .remove(&(channel.clone(), session_id.to_owned()));
    }

    /// Resolve the gate for one tool call. Tries the session-scoped
    /// entry first (a specific TUI instance answering for its own
    /// session) and falls back to the type-level gate (a sidecar that
    /// answers for every session). When neither is registered returns
    /// `AutoDenyGate` (fail-closed).
    pub fn get(&self, channel: &ChannelType, session_id: &str) -> Arc<dyn ApprovalGate> {
        if let Some(entry) = self
            .session_level
            .get(&(channel.clone(), session_id.to_owned()))
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

/// Thread-safe queue of pending approval prompts. Shared between a
/// [`ChannelApprovalGate`] (producer) and the channel's event loop
/// (consumer). Cloneable — both sides hold a handle.
#[derive(Clone)]
pub struct ApprovalQueue {
    inner: Arc<Mutex<VecDeque<PendingApproval>>>,
    resolver: Arc<Mutex<Option<ResolveFn>>>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
            resolver: Arc::new(Mutex::new(None)),
        }
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
        self.inner.lock().push_back(entry);
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
        let popped = self.inner.lock().pop_front();
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
    /// of submission order. Returns `true` when an entry matched.
    pub fn resolve_by_call_id(&self, call_id: &str, decision: ApprovalDecision) -> bool {
        let mut q = self.inner.lock();
        if let Some(pos) = q.iter().position(|e| e.req.call_id == call_id)
            && let Some(pending) = q.remove(pos)
        {
            if let Some(responder) = pending.responder {
                let _ = responder.send(decision);
            }
            return true;
        }
        false
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
        if let Some(pos) = q.iter().position(|e| e.req.call_id == call_id) {
            q.remove(pos);
            return true;
        }
        false
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
    waker: Arc<dyn Fn() + Send + Sync>,
    timeout: Duration,
}

impl ChannelApprovalGate {
    pub fn new(
        queue: ApprovalQueue,
        waker: Arc<dyn Fn() + Send + Sync>,
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
        let (tx, rx) = oneshot::channel();
        self.queue.push(PendingApproval {
            req,
            responder: Some(tx),
        });
        (self.waker)();
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(decision)) => decision,
            _ => ApprovalDecision::Deny,
        }
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
                call_id: "x".into(),
                session_id: "s".into(),
                user_id: "u".into(),
                tool: "t".into(),
                accesses: vec![],
                params_preview: String::new(),
            })
            .await;
        assert_eq!(out, ApprovalDecision::Deny);
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
            Arc::new(move || {
                woken2.store(true, std::sync::atomic::Ordering::SeqCst);
            }),
            Duration::from_secs(60),
        );

        let req = ApprovalRequest {
            call_id: "c1".into(),
            session_id: "s".into(),
            user_id: "u".into(),
            tool: "read".into(),
            accesses: vec![],
            params_preview: String::new(),
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
}
