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

use aura_channels::wire::Frame;
use aura_model::ChannelType;
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
/// agent-side caller that opens tunnels.
#[derive(Default)]
pub struct McpTunnelRouter {
    inbound: DashMap<String, TunnelEntry>,
}

struct TunnelEntry {
    channel_type: ChannelType,
    tx: mpsc::Sender<Vec<u8>>,
}

impl McpTunnelRouter {
    pub fn new() -> Self {
        Self::default()
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
        McpTunnel {
            tunnel_id,
            channel_type,
            rx,
            router: Arc::clone(self),
        }
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
    tunnel_id: String,
    channel_type: ChannelType,
    rx: mpsc::Receiver<Vec<u8>>,
    router: Arc<McpTunnelRouter>,
}

impl McpTunnel {
    pub fn id(&self) -> &str {
        &self.tunnel_id
    }

    pub fn channel_type(&self) -> &ChannelType {
        &self.channel_type
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
    pub async fn send(
        &self,
        control: &ChannelControlRegistry,
        payload: Vec<u8>,
    ) -> Result<(), McpTunnelError> {
        control
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

impl Drop for McpTunnel {
    fn drop(&mut self) {
        self.router.close_tunnel(&self.tunnel_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_returns_unique_tunnel_ids() {
        let router = Arc::new(McpTunnelRouter::new());
        let ct = ChannelType::from("lark");
        let a = router.open(ct.clone());
        let b = router.open(ct);
        assert_ne!(a.id(), b.id());
    }

    #[tokio::test]
    async fn forward_inbound_routes_to_matching_tunnel() {
        let router = Arc::new(McpTunnelRouter::new());
        let mut tunnel = router.open(ChannelType::from("lark"));
        assert!(router.forward_inbound(tunnel.id(), b"hello".to_vec()).await,);
        let received = tunnel.next_inbound().await;
        assert_eq!(received.as_deref(), Some(&b"hello"[..]));
    }

    #[tokio::test]
    async fn forward_inbound_drops_unknown_tunnel_silently() {
        let router = McpTunnelRouter::new();
        let delivered = router
            .forward_inbound("never-opened", b"orphan".to_vec())
            .await;
        assert!(!delivered);
    }

    #[tokio::test]
    async fn drain_for_channel_only_drops_matching_tunnels() {
        let router = Arc::new(McpTunnelRouter::new());
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
        let router = Arc::new(McpTunnelRouter::new());
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
        let router = Arc::new(McpTunnelRouter::new());
        let id = {
            let tunnel = router.open(ChannelType::from("lark"));
            tunnel.id().to_string()
        };
        // Tunnel dropped — registry entry should be gone.
        assert!(!router.forward_inbound(&id, b"x".to_vec()).await);
    }
}
