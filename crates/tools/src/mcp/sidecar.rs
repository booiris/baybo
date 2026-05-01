//! Sidecar MCP plumbing: byte-pipe transport + lazy per-session
//! discovery hook.
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
use aura_model::Session;
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{ToolDefinition, ToolOutput};

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

/// Lazy per-session discovery hook for sidecar-hosted MCP tools.
///
/// Sidecar MCP servers (e.g. lark's `feishu_get_chat_info`) are
/// scoped to a specific bot/tenant: a tool call from session A's
/// agent loop must not silently execute under session B's bot
/// credentials (slice 2A's structured tool error already enforces
/// this in the sidecar). To keep that scoping strict on the agent
/// side, sidecar tools are NOT eagerly registered into the global
/// [`crate::ToolRegistry`]. Instead, every agent loop turn asks the
/// provider "what tools are available for *this* session?" and the
/// provider materialises the per-session view.
///
/// Implementations decide their own caching policy. A typical
/// gateway-side implementation will:
///  * On first call for a session: open an MCP tunnel to the
///    sidecar serving `session.user.channel`, run the rmcp
///    handshake, and snapshot the advertised tools.
///  * Cache the snapshot per (channel_type, bot_id) so subsequent
///    turns reuse it.
///  * Return an empty vector when the session's channel has no
///    sidecar with `mcp_tunnel` capability — the agent loop just
///    proceeds with builtin + user-configured tools.
///
/// The trait is Send + Sync; `AgentLoop` holds it as
/// `Arc<dyn SidecarMcpProvider>`.
#[async_trait]
pub trait SidecarMcpProvider: Send + Sync {
    /// Tools the agent loop should expose to the LLM for `session`.
    /// Names must already be unique within the merged tool list —
    /// implementations should prefix with the channel/source (e.g.
    /// `lark/feishu_get_chat_info`) to avoid collisions with
    /// builtins or user-configured MCP tools.
    async fn tool_definitions_for_session(&self, session: &Session) -> Vec<ToolDefinition>;

    /// Dispatch a tool call to the sidecar's MCP server.
    ///
    /// Return value:
    ///  * `None` — `name` is not a sidecar tool for this session
    ///    (different channel prefix, no matching cached session).
    ///    The caller should fall back to the local `ToolRegistry`
    ///    dispatch path.
    ///  * `Some(Ok(output))` — the rmcp call returned a successful
    ///    `CallToolResult`.
    ///  * `Some(Err(reason))` — the rmcp call surfaced an error
    ///    (transport failure, server-side `isError`, schema
    ///    mismatch). `reason` is a sanitized diagnostic suitable
    ///    for the LLM's tool-result message.
    ///
    /// The session id is forwarded through `_meta.auraSessionId`
    /// on the underlying rmcp request so the sidecar can route to
    /// the right tenant in multi-bot deployments. (Slice 2A's
    /// structured tool error remains the safety net until sidecars
    /// implement the session→bot lookup.)
    async fn execute_for_session(
        &self,
        session: &Session,
        name: &str,
        params: Value,
    ) -> Option<Result<ToolOutput, String>>;
}

/// No-op provider: the AgentLoop default when no sidecar MCP is
/// wired in (TUI standalone, tests, gateway boots without any
/// `mcp_tunnel`-capable sidecar). Returns an empty tool list and
/// claims no tool dispatches.
pub struct NoSidecarMcp;

#[async_trait]
impl SidecarMcpProvider for NoSidecarMcp {
    async fn tool_definitions_for_session(&self, _session: &Session) -> Vec<ToolDefinition> {
        Vec::new()
    }

    async fn execute_for_session(
        &self,
        _session: &Session,
        _name: &str,
        _params: Value,
    ) -> Option<Result<ToolOutput, String>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aura_model::{ChannelType, User};
    use chrono::Utc;
    use rmcp::model::RequestId;
    use tokio::sync::Mutex;

    fn dummy_session() -> Session {
        let user = User {
            id: "u".into(),
            name: None,
            channel: ChannelType::tui(),
        };
        Session {
            id: "s".into(),
            user: user.clone(),
            channel: user.channel.clone(),
            messages: vec![],
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: Default::default(),
        }
    }

    #[tokio::test]
    async fn no_sidecar_mcp_returns_empty_and_does_not_dispatch() {
        let p = NoSidecarMcp;
        let session = dummy_session();
        assert!(
            p.tool_definitions_for_session(&session).await.is_empty(),
            "no-op provider must return zero tools"
        );
        assert!(
            p.execute_for_session(&session, "anything", Value::Null)
                .await
                .is_none(),
            "no-op provider must yield None so caller falls back to local tools"
        );
    }

    #[tokio::test]
    async fn provider_can_return_session_scoped_tools_and_dispatch() {
        struct Stub;
        #[async_trait]
        impl SidecarMcpProvider for Stub {
            async fn tool_definitions_for_session(
                &self,
                session: &Session,
            ) -> Vec<ToolDefinition> {
                vec![ToolDefinition {
                    name: format!("{}/feishu_get_chat_info", session.user.channel),
                    description: "stub".into(),
                    parameters_schema: serde_json::json!({"type": "object"}),
                }]
            }

            async fn execute_for_session(
                &self,
                session: &Session,
                name: &str,
                _params: Value,
            ) -> Option<Result<ToolOutput, String>> {
                let prefix = format!("{}/", session.user.channel);
                if name.starts_with(&prefix) {
                    Some(Ok(ToolOutput::Text(format!("called {name}"))))
                } else {
                    None
                }
            }
        }

        let p: Arc<dyn SidecarMcpProvider> = Arc::new(Stub);
        let session = dummy_session();
        let tools = p.tool_definitions_for_session(&session).await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "tui/feishu_get_chat_info");

        match p
            .execute_for_session(&session, "tui/feishu_get_chat_info", Value::Null)
            .await
        {
            Some(Ok(ToolOutput::Text(t))) => {
                assert_eq!(t, "called tui/feishu_get_chat_info");
            }
            other => panic!("expected Some(Ok(Text)), got {other:?}"),
        }
        assert!(
            p.execute_for_session(&session, "lark/feishu_get_chat_info", Value::Null)
                .await
                .is_none(),
            "wrong-channel prefix must yield None for fallback dispatch",
        );
    }

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

    /// Wire `connect_sidecar` against a fake MCP server running over
    /// the byte-pipe. The server replies to `initialize` + `tools/list`
    /// + `tools/call` and we verify the resulting `McpServerSession`
    /// can list and dispatch. The tools/call branch also asserts the
    /// `_meta.auraSessionId` injection lands at the server.
    #[tokio::test]
    async fn connect_sidecar_round_trips_initialize_and_tools_list() {
        use crate::mcp::transport::connect_sidecar;
        use serde_json::Value;

        // Outbound captures everything the rmcp client sends; the
        // server task reads from it and pushes responses back via
        // the inbound mpsc.
        let (server_in_tx, mut server_in_rx) = mpsc::channel::<Vec<u8>>(16);
        let (server_out_tx, server_out_rx) = mpsc::channel::<Vec<u8>>(16);

        struct PipeSender {
            tx: mpsc::Sender<Vec<u8>>,
        }
        #[async_trait]
        impl SidecarSender for PipeSender {
            async fn send(&self, payload: Vec<u8>) -> Result<(), String> {
                self.tx
                    .send(payload)
                    .await
                    .map_err(|e| format!("pipe closed: {e}"))
            }
        }

        // Fake server: read each inbound envelope, dispatch by method.
        let server = tokio::spawn(async move {
            while let Some(bytes) = server_in_rx.recv().await {
                let v: Value = serde_json::from_slice(&bytes).expect("client sends valid json");
                let method = v["method"].as_str().unwrap_or_default();
                match method {
                    "initialize" => {
                        let id = v["id"].clone();
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2024-11-05",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "stub", "version": "0.0.1"}
                            }
                        });
                        let _ = server_out_tx.send(serde_json::to_vec(&resp).unwrap()).await;
                    }
                    "notifications/initialized" => { /* notification; no reply */ }
                    "tools/list" => {
                        let id = v["id"].clone();
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [{
                                    "name": "stub_tool",
                                    "description": "test tool",
                                    "inputSchema": {"type": "object", "properties": {}}
                                }]
                            }
                        });
                        let _ = server_out_tx.send(serde_json::to_vec(&resp).unwrap()).await;
                    }
                    "tools/call" => {
                        // Echo the inbound `_meta.auraSessionId` back
                        // through the result text so the assertion
                        // below proves the slice 2A → 2E injection
                        // actually lands at the server.
                        let id = v["id"].clone();
                        let session_meta = v["params"]["_meta"]["auraSessionId"]
                            .as_str()
                            .unwrap_or("<missing>")
                            .to_string();
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": format!("session={session_meta}") }]
                            }
                        });
                        let _ = server_out_tx.send(serde_json::to_vec(&resp).unwrap()).await;
                    }
                    _ => { /* ignore */ }
                }
            }
        });

        let sender: Arc<dyn SidecarSender> = Arc::new(PipeSender { tx: server_in_tx });
        let session = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            connect_sidecar(sender, server_out_rx),
        )
        .await
        .expect("connect_sidecar within 5s")
        .expect("session ok");

        let tools = session.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "stub_tool");

        // Dispatch a tools/call with a session id and assert the
        // server saw it on `_meta.auraSessionId`.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.call_tool("stub_tool", serde_json::json!({}), Some("aura-session-42")),
        )
        .await
        .expect("call_tool within 5s")
        .expect("call_tool ok");
        match result {
            ToolOutput::Text(t) => assert_eq!(t, "session=aura-session-42"),
            other => panic!("expected Text, got {other:?}"),
        }

        session.shutdown().await;
        let _ = server.await;
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
