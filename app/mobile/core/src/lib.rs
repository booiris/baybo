//! **baybo-mobile-core** — the FFI-free shared client core for the Baybo mobile
//! companion.
//!
//! It runs in the Tauri shell's `src-tauri` (reached from the webview via Tauri
//! commands) and speaks the exact same protocol as the gateway by depending on
//! the same crates — [`wire`] for the `Frame` wire types and
//! [`device_proto`] for XXpsk0 pairing, Noise content/blob legs, and the AEAD
//! push-preview framing. No FFI, no
//! Tauri, no platform APIs here, so it is fully host-unit-testable and
//! cross-compiles to `aarch64-apple-ios` unchanged.

pub mod blob;
pub mod content;
pub mod error;
pub mod pairing;

pub use blob::{BlobDownload, BlobRequest, BlobResponse, BlobSession, DownloadStep};
pub use content::{
    ContentHandshake, ContentSession, subscribe_frame, user_message_frame, user_text_frame,
};
pub use error::MobileError;
pub use pairing::{
    PairAborted, PairChallenge, PairedGateway, PairedSummary, PairingClient, PairingRequest,
};

// Re-export the wire `Frame` (and the attachment types it carries) so the shell
// renders threads — and builds outbound attachment refs — against the same types
// the gateway emits.
pub use wire::{AttachmentKind, Frame, WireAttachment};
