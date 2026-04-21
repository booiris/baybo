//! Private WebSocket + MessagePack client used by the bundled TUI to
//! talk to the gateway's `/v1/channel-ws` endpoint.
//!
//! This is deliberately not a reusable SDK: the only channel consumer
//! outside the gateway itself is the TS package under
//! `sdks/channel-ts/`. Everything here is scoped to the TUI's
//! PSK-authenticated flow — the subprocess env-var flow has no Rust
//! consumer and isn't carried over.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aura_channels::wire::{self, Frame, Message, PROTOCOL_VERSION};
use aura_model::ChannelType;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{WebSocketStream, client_async};

/// HTTP header carrying the per-install TUI PSK (hex-encoded) on the WS
/// upgrade request. Mirrors `aura_gateway_auth::TUI_PSK_HEADER`; kept
/// as a plain const so the TUI stays free of a runtime dep on the
/// gateway-auth crate.
const TUI_PSK_HEADER: &str = "x-aura-tui-secret";

/// Nominal WebSocket URL. The UDS is the real transport, but
/// `tokio-tungstenite::client_async` still needs a syntactically valid
/// URL for the HTTP upgrade line; the host part is never resolved.
const HANDSHAKE_URL: &str = "ws://localhost/v1/channel-ws";

type WsStream = WebSocketStream<UnixStream>;
type WsSink = SplitSink<WsStream, WsMessage>;
type WsSource = SplitStream<WsStream>;

/// Error surface for the TUI's WS client.
#[derive(Debug, Error)]
pub enum WsClientError {
    #[error("uds dial failed: {0}")]
    UdsDial(#[source] std::io::Error),

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
/// Aura channel WebSocket.
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
    /// Connect using the bundled-TUI PSK flow: the handshake carries the
    /// hex-encoded PSK in `x-aura-tui-secret`, and the `Register` frame
    /// leaves the capability `token` empty because auth already happened
    /// on the upgrade request.
    pub async fn connect_tui(
        socket_path: impl AsRef<Path>,
        psk: &[u8; 32],
        channel_type: ChannelType,
    ) -> Result<Self, WsClientError> {
        let psk_hex = hex::encode(psk);
        Self::connect_inner(
            socket_path.as_ref().to_path_buf(),
            TUI_PSK_HEADER,
            &psk_hex,
            String::new(),
            channel_type,
        )
        .await
    }

    async fn connect_inner(
        socket_path: PathBuf,
        header_name: &'static str,
        header_value: &str,
        register_token: String,
        channel_type: ChannelType,
    ) -> Result<Self, WsClientError> {
        let stream = UnixStream::connect(&socket_path)
            .await
            .map_err(WsClientError::UdsDial)?;
        let mut request = HANDSHAKE_URL
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
        Ok(client)
    }

    async fn register(
        &self,
        token: String,
        channel_type: ChannelType,
    ) -> Result<(), WsClientError> {
        self.send_frame(&Frame::Register {
            token,
            channel_type,
            protocol_version: PROTOCOL_VERSION,
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

    /// Send one message to Aura.
    pub async fn send(&self, msg: Message) -> Result<(), WsClientError> {
        self.send_frame(&Frame::Message(msg)).await
    }

    /// Send a raw wire frame. Used for `Frame::ResolveApproval`; regular
    /// turn-output should prefer [`Self::send`].
    pub async fn send_raw(&self, frame: &Frame) -> Result<(), WsClientError> {
        self.send_frame(frame).await
    }

    /// Await the next raw wire frame from Aura. Exposes the full frame
    /// surface (`Delta`, `Notice`, approval events, …) so streaming and
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
