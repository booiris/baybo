//! Private WebSocket + MessagePack client used by the bundled TUI to
//! talk to the gateway's `/v1/channel-ws` endpoint.
//!
//! This is deliberately not a reusable SDK: the only channel consumer
//! outside the gateway itself is the TS package under
//! `sidecars/sdk/channel-ts/`. Everything here is scoped to the TUI's
//! token-authenticated flow — the gateway publishes a fresh per-start
//! token to the secret vault under `gateway.tui_token`; the TUI reads
//! it and presents it on the WS upgrade in the shared
//! `x-baybo-channel-token` header.

use std::net::SocketAddr;
use std::sync::Arc;

use baybo_channels::wire::{self, Frame, Message};
use baybo_model::{ChannelType, SessionId};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{WebSocketStream, client_async};

/// HTTP header carrying the channel-listener auth token on the WS
/// upgrade request. Mirrors `baybo_gateway::CHANNEL_TOKEN_HEADER`;
/// kept as a plain const so the TUI stays free of a runtime dep on
/// the gateway crate.
const CHANNEL_TOKEN_HEADER: &str = "x-baybo-channel-token";

type WsStream = WebSocketStream<TcpStream>;
type WsSink = SplitSink<WsStream, WsMessage>;
type WsSource = SplitStream<WsStream>;

/// Error surface for the TUI's WS client.
#[derive(Debug, Error)]
pub enum WsClientError {
    #[error("tcp dial failed: {0}")]
    TcpDial(#[source] std::io::Error),

    #[error("websocket upgrade failed: {0}")]
    WsUpgrade(String),

    #[error("registration rejected: {0}")]
    RegistrationRejected(String),

    #[error("wire: {0}")]
    Wire(#[from] wire::WireError),

    #[error("peer closed")]
    PeerClosed,

    #[error("protocol violation: {0}")]
    ProtocolViolation(&'static str),

    #[error("transport: {0}")]
    Transport(String),
}

/// Post-handshake handle for sending/receiving `Message`s over the
/// Baybo channel WebSocket.
///
/// Cheap to clone — the underlying sink and source each live behind
/// an `Arc<Mutex<..>>` so independent send/recv tasks can hold their
/// own `WsClient` clones without splitting the stream in user code.
#[derive(Clone)]
pub struct WsClient {
    sink: Arc<Mutex<WsSink>>,
    source: Arc<Mutex<WsSource>>,
}

impl WsClient {
    /// Connect using the bundled-TUI vault-token flow: the handshake
    /// presents the per-start TUI token in `x-baybo-channel-token`, and
    /// the `Register` frame leaves the capability `token` empty because
    /// auth already happened on the upgrade request. `session_id` pins
    /// this TUI instance to a specific session — the gateway routes
    /// approvals/output for that session to this WS only, so multiple
    /// TUIs can coexist on the same gateway without collision.
    ///
    /// Returns the client plus the server-side input-history snapshot
    /// the gateway pushes right after `RegisterAck` — consuming it here
    /// means the caller's `recv_*` loop sees a clean stream of turn
    /// frames afterward and never has to know the snapshot exists.
    pub async fn connect_tui(
        addr: SocketAddr,
        tui_token: &str,
        channel_type: ChannelType,
        session_id: SessionId,
    ) -> Result<(Self, Vec<String>), WsClientError> {
        Self::connect_inner(
            addr,
            CHANNEL_TOKEN_HEADER,
            tui_token,
            String::new(),
            channel_type,
            Some(session_id),
        )
        .await
    }

    async fn connect_inner(
        addr: SocketAddr,
        header_name: &'static str,
        header_value: &str,
        register_token: String,
        channel_type: ChannelType,
        session_id: Option<SessionId>,
    ) -> Result<(Self, Vec<String>), WsClientError> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(WsClientError::TcpDial)?;
        // Handshake URL embeds the dialled address so the HTTP
        // Upgrade's Host header matches what the gateway sees on the
        // accept socket — avoids any "origin mismatch" surprises if
        // axum ever gates on that.
        let handshake_url = format!("ws://{addr}/v1/channel-ws");
        let mut request = handshake_url
            .into_client_request()
            .map_err(|e| WsClientError::WsUpgrade(e.to_string()))?;
        request.headers_mut().insert(
            header_name,
            header_value.parse().map_err(|_| {
                WsClientError::WsUpgrade(format!("{header_name} is not a valid header"))
            })?,
        );
        let (ws, _) = client_async(request, stream)
            .await
            .map_err(|e| WsClientError::WsUpgrade(e.to_string()))?;
        let (sink, source) = ws.split();
        let client = Self {
            sink: Arc::new(Mutex::new(sink)),
            source: Arc::new(Mutex::new(source)),
        };
        client.register(register_token, channel_type).await?;
        // Register names the channel; per-session interest follows as a
        // separate Subscribe frame. TUI is selective-kind and always
        // wants exactly one session.
        let history = match &session_id {
            Some(sid) => {
                client.subscribe(sid.clone()).await?;
                client.recv_history_snapshot().await?
            }
            None => Vec::new(),
        };
        Ok((client, history))
    }

    async fn register(
        &self,
        token: String,
        channel_type: ChannelType,
    ) -> Result<(), WsClientError> {
        self.send_frame(&Frame::Register {
            token,
            channel_type,
        })
        .await?;

        match self.recv_frame().await? {
            Frame::RegisterAck { ok: true, .. } => Ok(()),
            Frame::RegisterAck { ok: false, reason } => Err(WsClientError::RegistrationRejected(
                reason.unwrap_or_else(|| "no reason given".to_string()),
            )),
            _ => Err(WsClientError::ProtocolViolation("expected RegisterAck")),
        }
    }

    async fn subscribe(&self, session_id: SessionId) -> Result<(), WsClientError> {
        self.send_frame(&Frame::Subscribe { session_id }).await
    }

    /// Read the one-shot `HistorySnapshot` frame the gateway sends right
    /// after `RegisterAck` for session-scoped TUI clients. Sidecars
    /// never receive this and must not call it.
    async fn recv_history_snapshot(&self) -> Result<Vec<String>, WsClientError> {
        match self.recv_frame().await? {
            Frame::HistorySnapshot { entries, .. } => Ok(entries),
            _ => Err(WsClientError::ProtocolViolation(
                "expected HistorySnapshot after RegisterAck",
            )),
        }
    }

    /// Send one message to Baybo.
    pub async fn send(&self, msg: Message) -> Result<(), WsClientError> {
        self.send_frame(&Frame::Message(msg)).await
    }

    /// Send a raw wire frame. Used for `Frame::ResolveApproval`; regular
    /// turn-output should prefer [`Self::send`].
    pub async fn send_raw(&self, frame: &Frame) -> Result<(), WsClientError> {
        self.send_frame(frame).await
    }

    /// Await the next raw wire frame from Baybo. Exposes the full frame
    /// surface (`AnswerDelta`, `Notice`, approval events, …) so streaming and
    /// approval-flow consumers can reconstruct them directly.
    pub async fn recv_any(&self) -> Result<Frame, WsClientError> {
        self.recv_frame().await
    }

    async fn send_frame(&self, frame: &Frame) -> Result<(), WsClientError> {
        let bytes = wire::encode(frame)?;
        let mut sink = self.sink.lock().await;
        sink.send(WsMessage::Binary(bytes))
            .await
            .map_err(|e| WsClientError::Transport(e.to_string()))
    }

    async fn recv_frame(&self) -> Result<Frame, WsClientError> {
        let mut source = self.source.lock().await;
        loop {
            match source.next().await {
                None => return Err(WsClientError::PeerClosed),
                Some(Err(e)) => return Err(WsClientError::Transport(e.to_string())),
                Some(Ok(msg)) => match msg {
                    WsMessage::Binary(bytes) => return Ok(wire::decode(&bytes)?),
                    WsMessage::Close(_) => return Err(WsClientError::PeerClosed),
                    // tokio-tungstenite auto-replies to pings; skip
                    // control / raw-frame variants and keep reading.
                    WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
                    WsMessage::Text(_) => {
                        return Err(WsClientError::ProtocolViolation("unexpected text frame"));
                    }
                },
            }
        }
    }
}
