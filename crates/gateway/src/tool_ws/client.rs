//! `WsBrowserSidecarClient` — implements [`BrowserSidecarClient`] by
//! marshalling RPCs onto the registered tool-ws connection. Holds:
//!  - the outbound channel of the currently-attached sidecar (or
//!    `None` when no sidecar is connected),
//!  - a pending-call table keyed by RPC id.
//!
//! The WS server (`server.rs`) calls [`WsBrowserSidecarClient::attach`]
//! after a successful Register handshake and
//! [`WsBrowserSidecarClient::detach`] on disconnect; in between, every
//! inbound `RpcResult` / `RpcError` is funneled through
//! [`WsBrowserSidecarClient::dispatch_inbound`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use aura_tools::builtin::browser::{BrowserRpcError, BrowserSidecarClient};
use dashmap::DashMap;
use parking_lot::RwLock;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::frame::ToolFrame;

type PendingTx = oneshot::Sender<Result<Value, BrowserRpcError>>;

/// Bound on the per-connection outbound queue. The writer task
/// drains this and pushes onto the WebSocket sink; a sidecar that
/// stalls causes back-pressure here rather than uncapped memory
/// growth. 256 frames is well above any plausible burst (each tool
/// call emits one RpcRequest; periodic Pong is one frame; events
/// are bounded sidecar-side at 256 too) and small enough to fail
/// fast when the sidecar genuinely hangs.
pub(crate) const OUTBOUND_CHAN_CAPACITY: usize = 256;

/// Opaque handle the route handler keeps for its connection.
/// `attach` returns one; `detach` only clears the active outbound
/// when the generation matches the currently-attached one — this
/// guarantees a stale connection's eventual close cannot tear down
/// a fresh sidecar that registered in the meantime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachGuard(u64);

#[derive(Clone)]
struct ActiveAttach {
    outbound: mpsc::Sender<ToolFrame>,
    generation: u64,
}

#[derive(Default)]
struct ClientState {
    /// Outbound queue feeding the currently-attached sidecar's WS
    /// sink. `None` means no sidecar is registered; every `call` in
    /// that state returns [`BrowserRpcError::Disconnected`] without
    /// queueing anything (we don't buffer RPCs across disconnects —
    /// the caller's tool surface decides whether to retry).
    outbound: RwLock<Option<ActiveAttach>>,
    pending: DashMap<String, PendingTx>,
    /// Monotonic generation. Each successful `attach` increments
    /// this and tags the new active outbound; a `detach` whose
    /// guard's generation doesn't match current is a no-op.
    next_generation: AtomicU64,
}

#[derive(Clone, Default)]
pub struct WsBrowserSidecarClient {
    state: Arc<ClientState>,
}

impl WsBrowserSidecarClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called by `server.rs` after the Register handshake succeeds.
    /// Returns an [`AttachGuard`] tagged with the current generation;
    /// the route handler must pass that guard back to [`detach`] on
    /// connection close. A second concurrent `attach` (operator
    /// reconnect, race between two valid tokens) replaces the active
    /// outbound and *bumps* the generation, so the old connection's
    /// eventual `detach` becomes a no-op and any in-flight RPCs the
    /// new connection just took over keep their pending entries.
    pub fn attach(&self, outbound: mpsc::Sender<ToolFrame>) -> AttachGuard {
        let generation = self.state.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut slot = self.state.outbound.write();
        *slot = Some(ActiveAttach {
            outbound,
            generation,
        });
        AttachGuard(generation)
    }

    /// Whether a sidecar is currently registered. Surfaced for status
    /// endpoints and for tests that need to wait on attach completion.
    pub fn is_attached(&self) -> bool {
        self.state.outbound.read().is_some()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn is_attached_for_test(&self) -> bool {
        self.is_attached()
    }

    /// Called by `server.rs` on connection close. Only clears the
    /// active outbound when `guard`'s generation matches the
    /// currently-attached one — a stale guard from a previous
    /// connection that has since been replaced is a no-op.
    ///
    /// In-flight RPCs are failed with `Disconnected` only when the
    /// detach actually takes effect; if a stale connection's close
    /// fires after the active sidecar already replaced it, we leave
    /// the live pending oneshots alone.
    pub fn detach(&self, guard: AttachGuard) {
        let cleared = {
            let mut slot = self.state.outbound.write();
            match slot.as_ref() {
                Some(active) if active.generation == guard.0 => {
                    *slot = None;
                    true
                }
                _ => false,
            }
        };
        if !cleared {
            return;
        }
        // Drain pending and fail. Use a short retain loop so we don't
        // hold the dashmap shard guard while resolving the oneshots.
        let keys: Vec<String> = self.state.pending.iter().map(|e| e.key().clone()).collect();
        for k in keys {
            if let Some((_, tx)) = self.state.pending.remove(&k) {
                let _ = tx.send(Err(BrowserRpcError::Disconnected));
            }
        }
    }

    /// Called by `server.rs` for every frame received from the sidecar.
    pub fn dispatch_inbound(&self, frame: ToolFrame) {
        match frame {
            ToolFrame::RpcResult { id, result } => {
                if let Some((_, tx)) = self.state.pending.remove(&id) {
                    let _ = tx.send(Ok(result));
                } else {
                    tracing::debug!(id = %id, "tool-ws: RpcResult for unknown call");
                }
            }
            ToolFrame::RpcError { id, code, message } => {
                if let Some((_, tx)) = self.state.pending.remove(&id) {
                    let _ = tx.send(Err(BrowserRpcError::Sidecar { code, message }));
                } else {
                    tracing::debug!(id = %id, code = %code, "tool-ws: RpcError for unknown call");
                }
            }
            ToolFrame::Event { name, data } => {
                tracing::info!(event = %name, ?data, "tool-ws: sidecar event");
            }
            ToolFrame::Ping { ts } => {
                let tx = self
                    .state
                    .outbound
                    .read()
                    .as_ref()
                    .map(|a| a.outbound.clone());
                if let Some(tx) = tx
                    && let Err(e) = tx.try_send(ToolFrame::Pong { ts })
                {
                    // try_send (not send().await) keeps `dispatch_inbound`
                    // synchronous; a full or closed queue means the
                    // sidecar is stalled — skip the pong and let the
                    // socket time out naturally.
                    tracing::debug!(?e, "tool-ws: pong drop (queue full or closed)");
                }
            }
            ToolFrame::Pong { .. } => {}
            other => {
                tracing::warn!(?other, "tool-ws: unexpected frame from sidecar");
            }
        }
    }
}

#[async_trait]
impl BrowserSidecarClient for WsBrowserSidecarClient {
    async fn call(
        &self,
        method: &str,
        params: Value,
        context_id: &str,
        timeout: Duration,
        cancellation_token: CancellationToken,
    ) -> Result<Value, BrowserRpcError> {
        let outbound = self
            .state
            .outbound
            .read()
            .as_ref()
            .map(|a| a.outbound.clone())
            .ok_or(BrowserRpcError::Disconnected)?;

        // Inject context_id into the params object so the sidecar
        // knows which BrowserContext to route to. If the caller passed
        // a non-object, wrap it so the sidecar always sees an object.
        let mut wrapped = match params {
            Value::Object(map) => Value::Object(map),
            other => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_string(), other);
                Value::Object(map)
            }
        };
        if let Value::Object(ref mut map) = wrapped {
            map.insert(
                "context_id".to_string(),
                Value::String(context_id.to_string()),
            );
        }

        let id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.state.pending.insert(id.clone(), tx);

        let frame = ToolFrame::RpcRequest {
            id: id.clone(),
            method: method.to_string(),
            params: wrapped,
        };
        // try_send so a stalled sidecar (writer not draining) fails
        // fast instead of letting the caller hang on an unbounded
        // queue. A full queue is treated the same as a disconnect
        // for the agent's purposes — both mean the call cannot make
        // progress. The Transport variant carries a dedicated message
        // so logs can distinguish the two.
        if let Err(e) = outbound.try_send(frame) {
            self.state.pending.remove(&id);
            return match e {
                mpsc::error::TrySendError::Full(_) => Err(BrowserRpcError::Transport(
                    "outbound queue full (sidecar stalled)".into(),
                )),
                mpsc::error::TrySendError::Closed(_) => Err(BrowserRpcError::Disconnected),
            };
        }

        let pending = Arc::clone(&self.state);
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                pending.pending.remove(&id);
                Err(BrowserRpcError::Cancelled)
            }
            _ = tokio::time::sleep(timeout) => {
                pending.pending.remove(&id);
                Err(BrowserRpcError::Timeout(timeout))
            }
            res = rx => match res {
                Ok(v) => v,
                Err(_) => Err(BrowserRpcError::Disconnected),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn call_without_attach_returns_disconnected() {
        let client = WsBrowserSidecarClient::new();
        let err = client
            .call(
                "navigate",
                json!({}),
                "s",
                Duration::from_secs(1),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, BrowserRpcError::Disconnected));
    }

    #[tokio::test]
    async fn call_with_pre_cancelled_token_returns_cancelled() {
        let client = WsBrowserSidecarClient::new();
        let (tx, mut rx) = mpsc::channel(super::OUTBOUND_CHAN_CAPACITY);
        let _guard = client.attach(tx);
        // Drain frames the call sends so the queue doesn't back up.
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = client
            .call("navigate", json!({}), "s", Duration::from_secs(5), cancel)
            .await
            .unwrap_err();
        assert!(
            matches!(err, BrowserRpcError::Cancelled),
            "expected Cancelled, got {err:?}"
        );
        // Pending entry must have been removed so it doesn't leak.
        assert_eq!(client.state.pending.len(), 0);
    }

    #[tokio::test]
    async fn call_returns_transport_when_outbound_full() {
        // Stalled sidecar (writer not draining the queue): the
        // bounded outbound mpsc fills up and `try_send` reports
        // Full. The agent must see a fast-failing Transport error
        // rather than hanging.
        let client = WsBrowserSidecarClient::new();
        // Capacity 1 channel, never drained.
        let (tx, _rx) = mpsc::channel::<ToolFrame>(1);
        let _guard = client.attach(tx);
        // First call lands the slot.
        let c1 = client.clone();
        let h1 = tokio::spawn(async move {
            c1.call(
                "x",
                json!({}),
                "s",
                Duration::from_millis(50),
                CancellationToken::new(),
            )
            .await
        });
        // Give the first call time to enqueue.
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Second call now hits a full queue.
        let err = client
            .call(
                "y",
                json!({}),
                "s",
                Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, BrowserRpcError::Transport(ref m) if m.contains("queue full")),
            "expected Transport queue-full, got {err:?}",
        );
        // First call still pending; let it time out.
        let r1 = h1.await.unwrap();
        assert!(matches!(r1, Err(BrowserRpcError::Timeout(_))));
    }

    #[tokio::test]
    async fn call_times_out_when_no_response() {
        let client = WsBrowserSidecarClient::new();
        let (tx, mut rx) = mpsc::channel(super::OUTBOUND_CHAN_CAPACITY);
        let _guard = client.attach(tx);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let err = client
            .call(
                "navigate",
                json!({}),
                "s",
                Duration::from_millis(50),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, BrowserRpcError::Timeout(_)),
            "expected Timeout, got {err:?}"
        );
        assert_eq!(client.state.pending.len(), 0);
    }

    #[tokio::test]
    async fn detach_fails_all_pending_calls_with_disconnected() {
        let client = WsBrowserSidecarClient::new();
        let (tx, _rx) = mpsc::channel(super::OUTBOUND_CHAN_CAPACITY);
        let guard = client.attach(tx);
        let c1 = client.clone();
        let c2 = client.clone();
        let h1 = tokio::spawn(async move {
            c1.call(
                "x",
                json!({}),
                "s",
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
        });
        let h2 = tokio::spawn(async move {
            c2.call(
                "y",
                json!({}),
                "s",
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
        });
        // Give both calls time to register pending entries.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(client.is_attached());
        client.detach(guard);
        assert!(!client.is_attached());
        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();
        assert!(matches!(r1, Err(BrowserRpcError::Disconnected)));
        assert!(matches!(r2, Err(BrowserRpcError::Disconnected)));
        assert_eq!(client.state.pending.len(), 0);
    }

    #[tokio::test]
    async fn stale_detach_after_replace_is_a_noop() {
        // Connection A registers, connection B replaces it (operator
        // reconnect). When A's WS finally closes and the route handler
        // calls detach(guard_A), the active sidecar (B) must NOT be
        // dropped and B's in-flight RPCs must NOT be failed.
        let client = WsBrowserSidecarClient::new();
        let (tx_a, _rx_a) = mpsc::channel(super::OUTBOUND_CHAN_CAPACITY);
        let guard_a = client.attach(tx_a);
        let (tx_b, mut rx_b) = mpsc::channel(super::OUTBOUND_CHAN_CAPACITY);
        let _guard_b = client.attach(tx_b);
        assert!(client.is_attached());

        // B has an in-flight call.
        let c2 = client.clone();
        let h = tokio::spawn(async move {
            c2.call(
                "x",
                json!({}),
                "s",
                Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
        });
        // Wait for the request to land in B's queue so we know the
        // call has registered its pending entry.
        let frame = rx_b.recv().await.expect("B should receive RpcRequest");

        // A's connection finally closes. Stale guard → no-op.
        client.detach(guard_a);
        assert!(
            client.is_attached(),
            "stale detach must not drop the active sidecar"
        );
        assert_eq!(
            client.state.pending.len(),
            1,
            "stale detach must not fail B's in-flight call"
        );

        // Wire B's response so the spawned call resolves cleanly.
        let id = match frame {
            ToolFrame::RpcRequest { id, .. } => id,
            other => panic!("expected RpcRequest, got {other:?}"),
        };
        client.dispatch_inbound(ToolFrame::RpcResult {
            id,
            result: json!({ "ok": true }),
        });
        let res = h.await.unwrap().unwrap();
        assert_eq!(res["ok"], true);
    }

    #[tokio::test]
    async fn dispatch_inbound_resolves_matching_id() {
        let client = WsBrowserSidecarClient::new();
        let (tx, mut rx) = mpsc::channel(super::OUTBOUND_CHAN_CAPACITY);
        let _guard = client.attach(tx);
        let c2 = client.clone();
        let h = tokio::spawn(async move {
            c2.call(
                "x",
                json!({}),
                "s",
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
        });
        // Pull the queued RpcRequest the call emits and feed back a matching result.
        let frame = rx.recv().await.expect("RpcRequest queued");
        let id = match &frame {
            ToolFrame::RpcRequest { id, .. } => id.clone(),
            other => panic!("expected RpcRequest, got {other:?}"),
        };
        client.dispatch_inbound(ToolFrame::RpcResult {
            id,
            result: json!({ "ok": true }),
        });
        let res = h.await.unwrap().expect("call resolved");
        assert_eq!(res["ok"], true);
    }

    #[tokio::test]
    async fn dispatch_inbound_rpc_error_is_surfaced() {
        let client = WsBrowserSidecarClient::new();
        let (tx, mut rx) = mpsc::channel(super::OUTBOUND_CHAN_CAPACITY);
        let _guard = client.attach(tx);
        let c2 = client.clone();
        let h = tokio::spawn(async move {
            c2.call(
                "x",
                json!({}),
                "s",
                Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
        });
        let frame = rx.recv().await.expect("RpcRequest queued");
        let id = match &frame {
            ToolFrame::RpcRequest { id, .. } => id.clone(),
            other => panic!("expected RpcRequest, got {other:?}"),
        };
        client.dispatch_inbound(ToolFrame::RpcError {
            id,
            code: "BOOM".into(),
            message: "nope".into(),
        });
        let err = h.await.unwrap().unwrap_err();
        match err {
            BrowserRpcError::Sidecar { code, message } => {
                assert_eq!(code, "BOOM");
                assert_eq!(message, "nope");
            }
            other => panic!("expected Sidecar error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_inbound_unknown_id_is_silent() {
        let client = WsBrowserSidecarClient::new();
        // No attach, no pending. Just confirm we don't panic on a
        // result for a call we don't know about.
        client.dispatch_inbound(ToolFrame::RpcResult {
            id: "ghost".into(),
            result: json!({}),
        });
        client.dispatch_inbound(ToolFrame::RpcError {
            id: "ghost".into(),
            code: "X".into(),
            message: "y".into(),
        });
    }

    #[tokio::test]
    async fn ping_triggers_pong_when_attached() {
        let client = WsBrowserSidecarClient::new();
        let (tx, mut rx) = mpsc::channel(super::OUTBOUND_CHAN_CAPACITY);
        let _guard = client.attach(tx);
        client.dispatch_inbound(ToolFrame::Ping { ts: 42 });
        let frame = rx.recv().await.expect("pong queued");
        match frame {
            ToolFrame::Pong { ts } => assert_eq!(ts, 42),
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[test]
    fn call_wraps_non_object_params_into_object() {
        // A tool that passes a bare scalar gets wrapped under
        // `value` so the sidecar always sees an object — keeps the
        // `context_id` injection unconditional.
        let client = WsBrowserSidecarClient::new();
        let (tx, _rx) = mpsc::channel(super::OUTBOUND_CHAN_CAPACITY);
        let _guard = client.attach(tx);
        // No async needed — the call_inner happens-once, but call()
        // is async. Use `block_on` via a runtime built explicitly.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = client
                .call(
                    "x",
                    serde_json::Value::String("hi".into()),
                    "s",
                    Duration::from_millis(10),
                    CancellationToken::new(),
                )
                .await;
        });
    }
}
