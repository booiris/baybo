use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aura_model::ChannelType;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{WebSocketStream, client_async};

use super::error::SdkError;
use super::wire::{self, Frame, Message, PROTOCOL_VERSION};

/// Env var: path of the channel UDS the sidecar must dial. Matches
/// the name Aura's gateway sets in `crates/gateway/src/spawn.rs`.
pub const ENV_CHANNEL_SOCKET: &str = "AURA_CHANNEL_SOCKET";

/// Env var: hex-encoded capability token the sidecar presents in the
/// [`Register`](super::wire::Frame::Register) frame.
pub const ENV_CHANNEL_TOKEN: &str = "AURA_CHANNEL_TOKEN";

/// HTTP header carrying the capability token on the WS upgrade request.
/// Mirrors `aura_gateway_auth::CHANNEL_TOKEN_HEADER` — duplicated here
/// as a plain `const` so the SDK stays free of a runtime dep on the
/// gateway-auth crate.
pub const CHANNEL_TOKEN_HEADER: &str = "x-aura-channel-token";

/// HTTP header carrying the per-install TUI PSK (hex-encoded) on the WS
/// upgrade request. Mirrors `aura_gateway_auth::TUI_PSK_HEADER`; kept
/// as a plain const for the same reason as [`CHANNEL_TOKEN_HEADER`].
pub const TUI_PSK_HEADER: &str = "x-aura-tui-secret";

/// Nominal WebSocket URL. The UDS is the real transport, but
/// `tokio-tungstenite::client_async` still needs a syntactically valid
/// URL for the HTTP upgrade line; the host part is never resolved.
const HANDSHAKE_URL: &str = "ws://localhost/v1/channel-ws";

type WsStream = WebSocketStream<UnixStream>;
type WsSink = SplitSink<WsStream, WsMessage>;
type WsSource = SplitStream<WsStream>;

/// Post-handshake handle for sending/receiving `Message`s over the
/// Aura channel WebSocket.
///
/// Cheap to clone — the underlying sink and source each live behind
/// an `Arc<Mutex<..>>` so independent send/recv tasks can hold their
/// own `Client` clones without splitting the stream in user code.
#[derive(Clone)]
pub struct Client {
    sink: Arc<Mutex<WsSink>>,
    source: Arc<Mutex<WsSource>>,
}

impl Client {
    /// Dial `$AURA_CHANNEL_SOCKET`, upgrade to WebSocket, run the
    /// Register/RegisterAck handshake with `$AURA_CHANNEL_TOKEN`, and
    /// return a ready `Client`. Intended for subprocess sidecars that
    /// Aura spawned and injected env vars into.
    pub async fn connect(channel_type: ChannelType) -> Result<Self, SdkError> {
        let socket_path =
            env::var(ENV_CHANNEL_SOCKET).map_err(|_| SdkError::MissingEnv(ENV_CHANNEL_SOCKET))?;
        let token =
            env::var(ENV_CHANNEL_TOKEN).map_err(|_| SdkError::MissingEnv(ENV_CHANNEL_TOKEN))?;
        Self::connect_inner(
            PathBuf::from(socket_path),
            CHANNEL_TOKEN_HEADER,
            &token,
            token.clone(),
            channel_type,
        )
        .await
    }

    /// Connect using the bundled-TUI PSK flow: the handshake carries the
    /// hex-encoded PSK in `x-aura-tui-secret`, and the `Register` frame
    /// leaves the capability `token` empty because auth already happened
    /// on the upgrade request.
    pub async fn connect_tui(
        socket_path: impl AsRef<Path>,
        psk: &[u8; 32],
        channel_type: ChannelType,
    ) -> Result<Self, SdkError> {
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
    ) -> Result<Self, SdkError> {
        let stream = UnixStream::connect(&socket_path)
            .await
            .map_err(SdkError::UdsDial)?;
        let mut request = HANDSHAKE_URL
            .into_client_request()
            .map_err(|e| SdkError::WsUpgrade(e.to_string()))?;
        request.headers_mut().insert(
            header_name,
            header_value
                .parse()
                .map_err(|_| SdkError::WsUpgrade(format!("{header_name} is not a valid header")))?,
        );
        let (ws, _) = client_async(request, stream)
            .await
            .map_err(|e| SdkError::WsUpgrade(e.to_string()))?;
        let (sink, source) = ws.split();
        let client = Self {
            sink: Arc::new(Mutex::new(sink)),
            source: Arc::new(Mutex::new(source)),
        };
        client.register(register_token, channel_type).await?;
        Ok(client)
    }

    async fn register(&self, token: String, channel_type: ChannelType) -> Result<(), SdkError> {
        self.send_frame(&Frame::Register {
            token,
            channel_type,
            protocol_version: PROTOCOL_VERSION,
        })
        .await?;

        match self.recv_frame().await? {
            Frame::RegisterAck { ok: true, .. } => Ok(()),
            Frame::RegisterAck { ok: false, reason } => Err(SdkError::RegistrationRejected(
                reason.unwrap_or_else(|| "no reason given".to_string()),
            )),
            _ => Err(SdkError::ProtocolViolation("expected RegisterAck")),
        }
    }

    /// Send one message to Aura.
    pub async fn send(&self, msg: Message) -> Result<(), SdkError> {
        self.send_frame(&Frame::Message(msg)).await
    }

    /// Send a raw wire frame. Used by the bundled TUI to issue
    /// [`Frame::ResolveApproval`] — sidecars that only exchange user
    /// messages should prefer [`Self::send`].
    pub async fn send_raw(&self, frame: &Frame) -> Result<(), SdkError> {
        self.send_frame(frame).await
    }

    /// Await the next raw wire frame from Aura. Exposes the full frame
    /// surface (`Delta`, `Notice`, approval events, …) so callers that
    /// need streaming or approval-flow data — the TUI in particular —
    /// can reconstruct them directly. Sidecars that only want the final
    /// per-turn `Message` should call [`Self::recv`] instead.
    pub async fn recv_any(&self) -> Result<Frame, SdkError> {
        self.recv_frame().await
    }

    /// Await the next message from Aura.
    ///
    /// Silently skips non-`Message` server-to-client frames (`Delta`,
    /// `Notice`, approval events) so sidecars that only care about the
    /// final turn output keep seeing one message per turn, as they did
    /// when `AgentOutput::Delta` was still coalesced into `Message`.
    /// Handshake frames mid-session are a contract break and surface as
    /// [`SdkError::ProtocolViolation`].
    pub async fn recv(&self) -> Result<Message, SdkError> {
        loop {
            match self.recv_frame().await? {
                Frame::Message(msg) => return Ok(msg),
                Frame::Delta { .. }
                | Frame::Notice { .. }
                | Frame::ApprovalRequested { .. }
                | Frame::ApprovalResolved { .. } => continue,
                Frame::Register { .. }
                | Frame::RegisterAck { .. }
                | Frame::ResolveApproval { .. } => {
                    return Err(SdkError::ProtocolViolation(
                        "unexpected frame kind post-handshake",
                    ));
                }
            }
        }
    }

    /// Close the WebSocket gracefully.
    pub async fn close(&self) -> Result<(), SdkError> {
        let mut sink = self.sink.lock().await;
        sink.close()
            .await
            .map_err(|e| SdkError::Transport(e.to_string()))
    }

    async fn send_frame(&self, frame: &Frame) -> Result<(), SdkError> {
        let bytes = wire::encode(frame)?;
        let mut sink = self.sink.lock().await;
        sink.send(WsMessage::Binary(bytes))
            .await
            .map_err(|e| SdkError::Transport(e.to_string()))
    }

    async fn recv_frame(&self) -> Result<Frame, SdkError> {
        let mut source = self.source.lock().await;
        loop {
            match source.next().await {
                None => return Err(SdkError::PeerClosed),
                Some(Err(e)) => return Err(SdkError::Transport(e.to_string())),
                Some(Ok(msg)) => match msg {
                    WsMessage::Binary(bytes) => return wire::decode(&bytes),
                    WsMessage::Close(_) => return Err(SdkError::PeerClosed),
                    // tokio-tungstenite auto-replies to pings; skip
                    // control / raw-frame variants and keep reading.
                    WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
                    WsMessage::Text(_) => {
                        return Err(SdkError::ProtocolViolation("unexpected text frame"));
                    }
                },
            }
        }
    }
}
