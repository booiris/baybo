//! Device client protocol core used by the iOS app.
//!
//! This used to live in `app/mobile/core` for the deleted Tauri companion. The
//! SwiftUI app is now the only mobile shell, so the thin protocol wrappers live
//! directly in the UniFFI crate while still depending on the shared wire and
//! device-proto crates.

pub mod api_tunnel;
pub mod content;
pub mod error;
pub mod pairing;

pub use api_tunnel::{
    ApiTunnelSession, MAX_TUNNEL_CHUNK, TunnelHeader, TunnelRequest, TunnelResponse,
    blob_id_sha256_hex,
};
pub use content::{
    ContentHandshake, ContentSession, register_device_frame, resolve_approval_frame,
    subscribe_frame, user_message_frame,
};
pub use error::MobileError;
pub use pairing::{PairChallenge, PairedSummary, PairingClient, PairingRequest};

pub use baybo_model::ApprovalDecision as WireApprovalDecision;
pub use wire::{AttachmentKind, Frame, WireAttachment, decode, encode};
