//! The UniFFI-facing surface: records, callback interfaces, and the flat error
//! enum. Everything Swift sees crosses through these types; the internal modules
//! keep their `Result<_, String>` prose and the transport's `TransportError`,
//! which [`BayboError::from_msg`] folds into the two load-bearing variants +
//! prose at the boundary.

use crate::binding::NOT_BOUND_MSG;
use crate::direct::INVALID_TOKEN_CODE;

/// The FFI error surface. `InvalidToken` and `NotBound` used to be string codes
/// the webview matched on (`invalid_token` / the unbound prose); as enum variants
/// the cross-language contract can't drift with a rewording.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum BayboError {
    /// The app holds neither a relay pairing nor direct credentials.
    #[error("{}", NOT_BOUND_MSG)]
    NotBound,
    /// The gateway rejected the admin Bearer token (HTTP 401).
    #[error("{}", INVALID_TOKEN_CODE)]
    InvalidToken,
    /// Any other failure, carrying the leg's own prose verbatim.
    #[error("{message}")]
    Other { message: String },
}

impl BayboError {
    /// Fold an internal `String` error into the FFI enum: the two stable codes
    /// become their variants, everything else rides as prose.
    pub(crate) fn from_msg(message: String) -> Self {
        match message.as_str() {
            INVALID_TOKEN_CODE => Self::InvalidToken,
            NOT_BOUND_MSG => Self::NotBound,
            _ => Self::Other { message },
        }
    }
}

/// APNs environment of this build. Passed in from Swift (from the Xcode build
/// configuration) instead of `cfg!(debug_assertions)`: the Rust core is usually
/// compiled in release even for a debug app, so a Rust-side cfg would misreport
/// `production` for a sandbox-token build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ApnsEnvironment {
    Sandbox,
    Production,
}

impl ApnsEnvironment {
    /// The wire string the gateway / pairing protocol expects.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Production => "production",
        }
    }

    pub(crate) fn to_pairing(self) -> device_proto::pairing::ApnsEnv {
        match self {
            Self::Sandbox => device_proto::pairing::ApnsEnv::Sandbox,
            Self::Production => device_proto::pairing::ApnsEnv::Production,
        }
    }
}

/// Construction-time configuration for [`crate::BayboClient`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct ClientConfig {
    /// Which APNs environment issued this build's tokens (Xcode debug builds →
    /// `Sandbox`, release/TestFlight → `Production`).
    pub apns_env: ApnsEnvironment,
    /// Directory for the rotating `baybo.log` (2 MiB × 3 files — the exportable
    /// log bundle). `None` disables file logging (host tests).
    pub log_dir: Option<String>,
}

/// Scan-to-pair target parsed from a `baybo://pair` QR payload.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PairTarget {
    /// Relay base URL (the QR's `h=`, already defaulted when absent).
    pub endpoint: String,
    /// Public rendezvous id (the QR's `r=`).
    pub rendezvous_id: String,
    /// Hex Noise PSK (the QR's `s=`); decoded + length-checked at `pair_begin`.
    pub secret: String,
    /// Relay admission key (the QR's `k=`), if present.
    pub remote_api_key: Option<String>,
}

/// What the confirm screen renders after `pair_begin`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairChallenge {
    pub device_id: String,
    pub confirm_code: String,
}

impl From<baybo_mobile_core::PairChallenge> for PairChallenge {
    fn from(c: baybo_mobile_core::PairChallenge) -> Self {
        Self {
            device_id: c.device_id,
            confirm_code: c.confirm_code,
        }
    }
}

/// What the UI renders after a successful pairing (never the secrets).
#[derive(Debug, Clone, uniffi::Record)]
pub struct PairedSummary {
    pub relay_node_id: String,
    pub rendezvous_id: String,
}

impl From<baybo_mobile_core::PairedSummary> for PairedSummary {
    fn from(s: baybo_mobile_core::PairedSummary) -> Self {
        Self {
            relay_node_id: s.relay_node_id,
            rendezvous_id: s.rendezvous_id,
        }
    }
}

/// A content-addressed attachment reference on an outbound message (already
/// uploaded via `blob_upload_bytes`).
#[derive(Debug, Clone, uniffi::Record)]
pub struct AttachmentRef {
    pub kind: AttachmentKind,
    pub blob_id: String,
    pub mime_type: String,
    pub size: u32,
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AttachmentKind {
    Image,
    Audio,
    File,
}

impl From<AttachmentRef> for baybo_mobile_core::WireAttachment {
    fn from(a: AttachmentRef) -> Self {
        let kind = match a.kind {
            AttachmentKind::Image => baybo_mobile_core::AttachmentKind::Image,
            AttachmentKind::Audio => baybo_mobile_core::AttachmentKind::Audio,
            AttachmentKind::File => baybo_mobile_core::AttachmentKind::File,
        };
        Self {
            kind,
            blob_id: a.blob_id,
            mime_type: a.mime_type,
            size: a.size,
            filename: a.filename,
        }
    }
}

/// Where a live chat session's frames land. One sink per `chat_connect`; calls
/// arrive on the core's tokio workers, so the Swift implementation must be
/// thread-safe (hop to the main actor before touching UI).
#[uniffi::export(with_foreign)]
pub trait FrameSink: Send + Sync {
    /// One inbound `wire::Frame`, serialized as JSON (the same shape the web
    /// transcript already consumes). `Ping` is answered inside the pump and never
    /// surfaces here.
    fn on_frame(&self, frame_json: String);

    /// The session ended ON ITS OWN (peer closed, liveness watchdog, Noise
    /// desync). Deliberate teardown — a fresh connect, `chat_disconnect`,
    /// `logout` — aborts the pump before this fires, so it signals only
    /// unsolicited death; the owner reconnects with backoff.
    fn on_disconnected(&self, session_id: String);
}

/// Gateway-side cancellation of an in-flight pairing (the operator declined or
/// the link dropped) while the confirm screen is up — dismiss it instead of
/// hanging until the user taps.
#[uniffi::export(with_foreign)]
pub trait PairAbortListener: Send + Sync {
    fn on_abort(&self, reason: String);
}
