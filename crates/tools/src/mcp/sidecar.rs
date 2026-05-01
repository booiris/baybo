//! rmcp `Transport` over an opaque byte-pipe.
//!
//! Sidecar-hosted MCP servers (Phase 3.3 slice 2B) run inside a
//! channel sidecar and exchange JSON-RPC envelopes with the gateway
//! via [`Frame::Mcp`](aura_channels::wire::Frame). The gateway's
//! `McpTunnel` wraps that frame flow into a tunnel-scoped sender +
//! inbound receiver pair.
//!
//! This module is the rmcp side of that bridge. It implements
//! [`rmcp::transport::Transport`] for `RoleClient` over:
//!  * a [`SidecarSender`] (cheaply cloneable handle for outbound), and
//!  * an [`mpsc::Receiver<Vec<u8>>`] for inbound bytes.
//!
//! Encoding is plain JSON (one envelope per `send` / `receive`),
//! matching the sidecar transport at
//! `channel-src/lark/src/mcp/transport.ts`.
//!
//! The split between `Sender` (Arc-clonable) and the inbound `&mut`
//! receiver is forced by rmcp's trait shape: `Transport::send`
//! returns a `'static` future, so it cannot borrow `&mut self`. We
//! clone the sender into the future; the receiver stays inside
//! `&mut self` for the sequential `receive` path.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum SidecarTransportError {
    #[error("encode JSON-RPC envelope: {0}")]
    Encode(String),
    #[error("send to sidecar: {0}")]
    Send(String),
}

/// Outbound half. Cloneable so the rmcp `Transport::send` future can
/// own its own handle (the future must be `'static`).
#[async_trait]
pub trait SidecarSender: Send + Sync {
    /// Push a JSON-RPC envelope to the sidecar. Returns `Err(reason)`
    /// when the underlying tunnel is closed (sidecar disconnected,
    /// channel rotated, etc).
    async fn send(&self, payload: Vec<u8>) -> Result<(), String>;
}

pub struct SidecarTransport {
    sender: Arc<dyn SidecarSender>,
    inbound: mpsc::Receiver<Vec<u8>>,
}

impl SidecarTransport {
    pub fn new(sender: Arc<dyn SidecarSender>, inbound: mpsc::Receiver<Vec<u8>>) -> Self {
        Self { sender, inbound }
    }
}

impl Transport<RoleClient> for SidecarTransport {
    type Error = SidecarTransportError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let sender = Arc::clone(&self.sender);
        let encoded = serde_json::to_vec(&item).map_err(|e| SidecarTransportError::Encode(e.to_string()));
        async move {
            let bytes = encoded?;
            sender
                .send(bytes)
                .await
                .map_err(SidecarTransportError::Send)
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            let bytes = self.inbound.recv().await?;
            match serde_json::from_slice::<RxJsonRpcMessage<RoleClient>>(&bytes) {
                Ok(msg) => return Some(msg),
                Err(e) => {
                    // A malformed envelope from the sidecar is a
                    // protocol bug; drop it and keep going so a
                    // single bad frame doesn't kill the session.
                    tracing::warn!(
                        error = %e,
                        len = bytes.len(),
                        "sidecar mcp: dropping malformed json-rpc envelope",
                    );
                }
            }
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inbound.close();
        std::future::ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rmcp::model::RequestId;
    use tokio::sync::Mutex;

    struct CapturingSender {
        out: Arc<Mutex<Vec<Vec<u8>>>>,
        fail: bool,
    }

    #[async_trait]
    impl SidecarSender for CapturingSender {
        async fn send(&self, payload: Vec<u8>) -> Result<(), String> {
            if self.fail {
                return Err("tunnel closed".into());
            }
            self.out.lock().await.push(payload);
            Ok(())
        }
    }

    fn sample_tools_list() -> TxJsonRpcMessage<RoleClient> {
        // `tools/list` round-trips through serde without touching any
        // non-exhaustive rmcp constructors, so build it from JSON.
        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list"
        });
        serde_json::from_value(raw).expect("tools/list parses as TxJsonRpcMessage")
    }

    #[tokio::test]
    async fn send_serialises_and_forwards_to_sender() {
        let out = Arc::new(Mutex::new(Vec::new()));
        let sender = Arc::new(CapturingSender {
            out: Arc::clone(&out),
            fail: false,
        });
        let (_tx, rx) = mpsc::channel(1);
        let mut transport = SidecarTransport::new(sender, rx);

        transport.send(sample_tools_list()).await.expect("send ok");

        let captured = out.lock().await;
        assert_eq!(captured.len(), 1);
        let parsed: serde_json::Value = serde_json::from_slice(&captured[0]).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["method"], "tools/list");
    }

    #[tokio::test]
    async fn send_propagates_pipe_errors() {
        let sender = Arc::new(CapturingSender {
            out: Arc::new(Mutex::new(Vec::new())),
            fail: true,
        });
        let (_tx, rx) = mpsc::channel(1);
        let mut transport = SidecarTransport::new(sender, rx);

        let err = transport.send(sample_tools_list()).await.unwrap_err();
        assert!(matches!(err, SidecarTransportError::Send(_)));
    }

    #[tokio::test]
    async fn receive_decodes_envelope_and_skips_garbage() {
        let sender = Arc::new(CapturingSender {
            out: Arc::new(Mutex::new(Vec::new())),
            fail: false,
        });
        let (tx, rx) = mpsc::channel(4);
        let mut transport = SidecarTransport::new(sender, rx);

        // A malformed envelope must be dropped (logged at warn) so a
        // single bad frame doesn't kill the session.
        tx.send(b"not-json".to_vec()).await.unwrap();

        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "tools": [] }
        });
        tx.send(serde_json::to_vec(&resp).unwrap()).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), transport.receive())
            .await
            .expect("receive resolved")
            .expect("got some message");
        match received {
            RxJsonRpcMessage::<RoleClient>::Response(r) => {
                assert!(matches!(r.id, RequestId::Number(1)));
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn receive_returns_none_when_inbound_closes() {
        let sender = Arc::new(CapturingSender {
            out: Arc::new(Mutex::new(Vec::new())),
            fail: false,
        });
        let (tx, rx) = mpsc::channel(1);
        let mut transport = SidecarTransport::new(sender, rx);
        drop(tx);
        assert!(transport.receive().await.is_none());
    }

    #[tokio::test]
    async fn close_drains_then_ends_inbound() {
        let sender = Arc::new(CapturingSender {
            out: Arc::new(Mutex::new(Vec::new())),
            fail: false,
        });
        let (tx, rx) = mpsc::channel(2);
        let mut transport = SidecarTransport::new(sender, rx);

        // Pre-buffer a valid envelope, then close. close() shuts the
        // sender side so further sends fail, but already-buffered
        // items still drain — matches `mpsc::Receiver::close`.
        tx.send(
            serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 7,
                "result": { "tools": [] }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        transport.close().await.expect("close ok");
        assert!(transport.receive().await.is_some(), "buffered envelope drains after close");

        // After the buffered item, the next send is rejected and the
        // receiver ends.
        assert!(tx.send(b"x".to_vec()).await.is_err());
        assert!(transport.receive().await.is_none(), "closed inbound returns None");
    }
}
