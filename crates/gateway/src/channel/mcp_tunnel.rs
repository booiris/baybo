//! MCP transport between Aura and a sidecar's hosted MCP server.
//!
//! Per Decision #9 of the Lark report: JSON-RPC envelopes ride the
//! existing channel WS as opaque [`Frame::Mcp`] payloads, gateway
//! forwards byte-for-byte without parsing. Avoids a separate transport
//! and keeps the admin auth surface untouched; future MCP protocol
//! versions are transparent (no wire bump).
//!
//! This module is the gateway's traffic cop:
//!
//! - The agent side (Aura's MCP client adapter, lands in slice 2)
//!   calls [`McpTunnelRouter::open`] to mint a `tunnel_id`. Each
//!   inbound `Frame::Mcp` matching that id is delivered to the
//!   tunnel's receiver. Outbound bytes go through
//!   [`McpTunnel::send`] which wraps them in `Frame::Mcp` and pushes
//!   via [`ChannelControlRegistry`].
//!
//! - The sidecar side speaks `Frame::Mcp` natively over its WS pump.
//!   The inbound loop calls [`McpTunnelRouter::forward_inbound`] for
//!   each frame; unknown tunnel ids drop silently (typical when the
//!   agent timed out and unregistered before the late reply arrived).
//!
//! Capability `mcp_tunnel` gates the frame: peers that don't claim it
//! never see one, and a sidecar emitting one without claiming
//! support is a protocol violation (handled in route.rs).

use std::sync::Arc;

use async_trait::async_trait;
use aura_channels::wire::Frame;
use aura_model::ChannelType;
use aura_tools::mcp::SidecarSender;
use dashmap::DashMap;
use thiserror::Error;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::control::{ChannelControlError, ChannelControlRegistry};

/// Capacity of the per-tunnel inbound buffer. JSON-RPC envelopes
/// arrive interleaved with concurrent agent calls; 64 covers a
/// realistic burst without back-pressuring the WS pump.
const TUNNEL_BUFFER: usize = 64;

#[derive(Debug, Error)]
pub enum McpTunnelError {
    #[error("control plane error: {0}")]
    Control(#[from] ChannelControlError),
    #[error("tunnel '{0}' is no longer registered")]
    UnknownTunnel(String),
}

/// Shared registry. One per gateway process; threaded into both
/// `WsChannelState` (for the inbound forwarding path) and the
/// agent-side caller that opens tunnels. Holds an Arc to
/// [`ChannelControlRegistry`] so opened tunnels are self-contained
/// — `McpTunnel::send` doesn't need the registry threaded through
/// every call site.
pub struct McpTunnelRouter {
    inbound: DashMap<String, TunnelEntry>,
    control: Arc<ChannelControlRegistry>,
}

struct TunnelEntry {
    channel_type: ChannelType,
    tx: mpsc::Sender<Vec<u8>>,
}

impl McpTunnelRouter {
    pub fn new(control: Arc<ChannelControlRegistry>) -> Self {
        Self {
            inbound: DashMap::new(),
            control,
        }
    }

    /// Mint a new tunnel against `channel_type`. The returned
    /// [`McpTunnel`] carries the receiver half; the caller owns it
    /// for the tunnel's lifetime and drops to close.
    pub fn open(self: &Arc<Self>, channel_type: ChannelType) -> McpTunnel {
        let tunnel_id = Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::channel(TUNNEL_BUFFER);
        self.inbound.insert(
            tunnel_id.clone(),
            TunnelEntry {
                channel_type: channel_type.clone(),
                tx,
            },
        );
        let sender = Arc::new(McpTunnelSender {
            tunnel_id: tunnel_id.clone(),
            channel_type: channel_type.clone(),
            control: Arc::clone(&self.control),
        });
        let guard = McpTunnelGuard {
            tunnel_id,
            router: Arc::clone(self),
        };
        McpTunnel { sender, rx, guard }
    }

    /// Forward an inbound `Frame::Mcp` payload from a sidecar to its
    /// matching tunnel. Returns `false` when the tunnel is gone
    /// (agent unregistered between request and reply) — the frame is
    /// dropped silently in that case.
    ///
    /// The DashMap read guard is dropped *before* awaiting the mpsc
    /// send; otherwise a full inbound buffer + concurrent tunnel
    /// drop would deadlock. `Drop::close_tunnel` calls
    /// `DashMap::remove`, which blocks on outstanding read guards;
    /// holding one across the bounded-channel `send().await` lets a
    /// blocked send pin the guard while the would-be cleanup waits
    /// behind it.
    pub async fn forward_inbound(&self, tunnel_id: &str, payload: Vec<u8>) -> bool {
        let tx = {
            let Some(entry) = self.inbound.get(tunnel_id) else {
                return false;
            };
            entry.value().tx.clone()
        };
        tx.send(payload).await.is_ok()
    }

    /// Drop every tunnel routed through `channel_type`. Called when
    /// the sidecar disconnects so the agent-side `next_inbound`
    /// awaiters wake immediately with a closed-channel error rather
    /// than blocking until they're individually cancelled.
    pub fn drain_for_channel(&self, channel_type: &ChannelType) {
        self.inbound
            .retain(|_, entry| entry.channel_type != *channel_type);
    }

    fn close_tunnel(&self, tunnel_id: &str) {
        self.inbound.remove(tunnel_id);
    }
}

/// One open tunnel. The agent side awaits incoming envelopes via
/// [`Self::next_inbound`] and pushes outgoing envelopes via
/// [`Self::send`]. Drop the handle to close — the registry entry is
/// removed and any in-flight `forward_inbound` returns `false`.
pub struct McpTunnel {
    sender: Arc<McpTunnelSender>,
    rx: mpsc::Receiver<Vec<u8>>,
    guard: McpTunnelGuard,
}

impl McpTunnel {
    pub fn id(&self) -> &str {
        &self.sender.tunnel_id
    }

    pub fn channel_type(&self) -> &ChannelType {
        &self.sender.channel_type
    }

    /// Wait for the next inbound JSON-RPC envelope. Returns `None`
    /// when the tunnel was drained (sidecar disconnected) or the
    /// caller dropped its half explicitly.
    pub async fn next_inbound(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }

    /// Push an outgoing JSON-RPC envelope to the sidecar. Returns
    /// `Control(NotConnected)` when the sidecar isn't currently
    /// registered — the agent-side caller should treat this as
    /// transient and retry on the next reconcile tick.
    pub async fn send(&self, payload: Vec<u8>) -> Result<(), McpTunnelError> {
        self.sender.send_frame(payload).await
    }

    /// Decompose the tunnel into the parts an rmcp [`Transport`]
    /// needs: a cloneable sender (so the `'static` send-future can
    /// own its handle), the inbound receiver, and the registry-
    /// cleanup guard. Keep the guard alive for the transport's
    /// lifetime — dropping it closes the tunnel registry entry.
    ///
    /// [`Transport`]: rmcp::transport::Transport
    pub fn into_transport_parts(
        self,
    ) -> (
        Arc<dyn SidecarSender>,
        mpsc::Receiver<Vec<u8>>,
        McpTunnelGuard,
    ) {
        let Self { sender, rx, guard } = self;
        (sender, rx, guard)
    }
}

/// Outbound half of a tunnel. Cloneable + `Send + Sync` so the rmcp
/// `Transport::send` future can own a handle.
pub struct McpTunnelSender {
    tunnel_id: String,
    channel_type: ChannelType,
    control: Arc<ChannelControlRegistry>,
}

impl McpTunnelSender {
    async fn send_frame(&self, payload: Vec<u8>) -> Result<(), McpTunnelError> {
        self.control
            .send(
                &self.channel_type,
                Frame::Mcp {
                    tunnel_id: self.tunnel_id.clone(),
                    payload,
                },
            )
            .await
            .map_err(McpTunnelError::from)
    }
}

#[async_trait]
impl SidecarSender for McpTunnelSender {
    async fn send(&self, payload: Vec<u8>) -> Result<(), String> {
        self.send_frame(payload).await.map_err(|e| e.to_string())
    }
}

/// RAII handle that unregisters the tunnel from the router on drop.
/// Held inside `McpTunnel`, or transferred out via
/// [`McpTunnel::into_transport_parts`] when an rmcp transport owns
/// the tunnel's lifetime.
pub struct McpTunnelGuard {
    tunnel_id: String,
    router: Arc<McpTunnelRouter>,
}

impl Drop for McpTunnelGuard {
    fn drop(&mut self) {
        self.router.close_tunnel(&self.tunnel_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_router() -> Arc<McpTunnelRouter> {
        let control = Arc::new(ChannelControlRegistry::new());
        Arc::new(McpTunnelRouter::new(control))
    }

    #[tokio::test]
    async fn open_returns_unique_tunnel_ids() {
        let router = test_router();
        let ct = ChannelType::from("lark");
        let a = router.open(ct.clone());
        let b = router.open(ct);
        assert_ne!(a.id(), b.id());
    }

    #[tokio::test]
    async fn forward_inbound_routes_to_matching_tunnel() {
        let router = test_router();
        let mut tunnel = router.open(ChannelType::from("lark"));
        assert!(router.forward_inbound(tunnel.id(), b"hello".to_vec()).await,);
        let received = tunnel.next_inbound().await;
        assert_eq!(received.as_deref(), Some(&b"hello"[..]));
    }

    #[tokio::test]
    async fn forward_inbound_drops_unknown_tunnel_silently() {
        let router = test_router();
        let delivered = router
            .forward_inbound("never-opened", b"orphan".to_vec())
            .await;
        assert!(!delivered);
    }

    #[tokio::test]
    async fn drain_for_channel_only_drops_matching_tunnels() {
        let router = test_router();
        let mut lark = router.open(ChannelType::from("lark"));
        let mut weixin = router.open(ChannelType::from("weixin"));

        router.drain_for_channel(&ChannelType::from("lark"));

        // Lark tunnel is gone; weixin survives.
        assert!(!router.forward_inbound(lark.id(), b"x".to_vec()).await);
        assert!(router.forward_inbound(weixin.id(), b"y".to_vec()).await);
        assert_eq!(weixin.next_inbound().await.as_deref(), Some(&b"y"[..]));

        // The drained tunnel's receiver wakes with `None` (sender
        // dropped) instead of blocking forever.
        assert_eq!(lark.next_inbound().await, None);
    }

    /// Codex review regression: `forward_inbound` previously held a
    /// DashMap read guard across `tx.send().await`. With a full
    /// buffer the send blocked waiting for the receiver to drain;
    /// dropping the tunnel called `DashMap::remove` which itself
    /// blocked behind the held read guard. Result: stuck WS inbound
    /// task. Verify the cleanup path completes when the buffer is
    /// full and the tunnel is dropped concurrently.
    #[tokio::test]
    async fn forward_inbound_does_not_deadlock_when_buffer_full_and_tunnel_drops() {
        let router = test_router();
        let tunnel = router.open(ChannelType::from("lark"));
        let id = tunnel.id().to_string();

        // Fill the buffer. Sends complete because no one's reading
        // but nothing is blocked yet.
        for _ in 0..super::TUNNEL_BUFFER {
            assert!(router.forward_inbound(&id, vec![0]).await);
        }

        // Spawn a forward that will block on a full buffer.
        let router_clone = Arc::clone(&router);
        let id_for_send = id.clone();
        let blocked_send =
            tokio::spawn(async move { router_clone.forward_inbound(&id_for_send, vec![1]).await });

        // Yield so the spawned task gets to its first await.
        tokio::task::yield_now().await;

        // Drop the tunnel handle. Drop runs `close_tunnel` which
        // does `DashMap::remove` — must NOT block behind the read
        // guard the forward held.
        drop(tunnel);

        // The blocked send should now complete because the receiver
        // half was dropped (channel closed → send returns Err →
        // forward_inbound returns false).
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), blocked_send)
            .await
            .expect("forward_inbound deadlocked when tunnel dropped concurrently")
            .unwrap();
        assert!(!result, "send into a dropped tunnel must return false");
    }

    #[tokio::test]
    async fn dropping_tunnel_unregisters_it() {
        let router = test_router();
        let id = {
            let tunnel = router.open(ChannelType::from("lark"));
            tunnel.id().to_string()
        };
        // Tunnel dropped — registry entry should be gone.
        assert!(!router.forward_inbound(&id, b"x".to_vec()).await);
    }

    /// `into_transport_parts` hands the cleanup guard to the caller;
    /// the registry entry stays alive as long as the guard does.
    #[tokio::test]
    async fn into_transport_parts_keeps_tunnel_alive_via_guard() {
        let router = test_router();
        let tunnel = router.open(ChannelType::from("lark"));
        let id = tunnel.id().to_string();
        let (sender, _rx, guard) = tunnel.into_transport_parts();

        // Tunnel is still routable while the guard lives, even
        // though the original `McpTunnel` was decomposed.
        assert!(router.forward_inbound(&id, b"x".to_vec()).await);
        // Sender remains usable across clones (rmcp `Transport::send`
        // future requires `'static`).
        let _ = Arc::clone(&sender);

        drop(guard);
        // After the guard drops the registry entry is gone.
        assert!(!router.forward_inbound(&id, b"y".to_vec()).await);
    }

    #[tokio::test]
    async fn sender_send_returns_not_connected_error_string() {
        let router = test_router();
        let tunnel = router.open(ChannelType::from("lark"));
        let (sender, _rx, _guard) = tunnel.into_transport_parts();
        // No sidecar registered for `lark`, so SidecarSender::send
        // surfaces the `NotConnected` error as a string.
        let err = sender.send(b"x".to_vec()).await.unwrap_err();
        assert!(err.contains("no sidecar"), "got: {err}");
    }

    /// End-to-end wiring: register a fake sidecar pump on the
    /// control registry, then drive the tunnel through an
    /// `aura_tools::SidecarTransport`. Outbound rmcp messages
    /// arrive as `Frame::Mcp` with the right tunnel_id; inbound
    /// bytes round-trip through `forward_inbound`.
    #[tokio::test]
    async fn end_to_end_through_sidecar_transport() {
        use aura_tools::mcp::SidecarTransport;
        use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
        use rmcp::transport::Transport;

        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(Arc::clone(&control)));
        let ct = ChannelType::from("lark");

        // Fake sidecar pump.
        let (pump_tx, mut pump_rx) = mpsc::channel::<Frame>(8);
        control.register(ct.clone(), pump_tx);

        let tunnel = router.open(ct.clone());
        let tunnel_id = tunnel.id().to_string();
        let (sender, rx, _guard) = tunnel.into_transport_parts();
        let mut transport = SidecarTransport::new(sender, rx);

        // Outbound: a request goes through, lands on the pump as
        // Frame::Mcp with the right tunnel_id and a JSON payload.
        let req: TxJsonRpcMessage<RoleClient> = serde_json::from_value(
            serde_json::json!({"jsonrpc":"2.0","id":42,"method":"tools/list"}),
        )
        .unwrap();
        transport.send(req).await.expect("transport send");

        let frame = pump_rx.recv().await.expect("pump receives frame");
        match frame {
            Frame::Mcp {
                tunnel_id: id,
                payload,
            } => {
                assert_eq!(id, tunnel_id);
                let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                assert_eq!(v["id"], 42);
                assert_eq!(v["method"], "tools/list");
            }
            other => panic!("unexpected outbound frame: {other:?}"),
        }

        // Inbound: bytes pushed through forward_inbound resurface as
        // a typed Response on the rmcp side.
        let resp_bytes = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": { "tools": [] }
        }))
        .unwrap();
        assert!(router.forward_inbound(&tunnel_id, resp_bytes).await);
        let received = tokio::time::timeout(std::time::Duration::from_secs(1), transport.receive())
            .await
            .expect("receive resolves")
            .expect("got message");
        assert!(matches!(
            received,
            RxJsonRpcMessage::<RoleClient>::Response(_)
        ));
    }
}
