use thiserror::Error;

/// Error surface for the WebSocket + MessagePack channel SDK.
///
/// Kept intentionally separate from `ChannelError` — this one targets
/// plugin/sidecar authors, whereas `ChannelError` is the in-process
/// adapter registry's surface. `WsUpgrade` and `Transport` carry their
/// messages as `String` so the enum stays stable when the underlying
/// transport error type changes.
#[derive(Debug, Error)]
pub enum SdkError {
    #[error("missing env var {0}")]
    MissingEnv(&'static str),

    #[error("uds dial failed: {0}")]
    UdsDial(#[source] std::io::Error),

    #[error("websocket upgrade failed: {0}")]
    WsUpgrade(String),

    #[error("registration rejected: {0}")]
    RegistrationRejected(String),

    #[error("encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),

    #[error("decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),

    #[error("peer closed")]
    PeerClosed,

    #[error("protocol violation: {0}")]
    ProtocolViolation(&'static str),

    #[error("transport: {0}")]
    Transport(String),
}
