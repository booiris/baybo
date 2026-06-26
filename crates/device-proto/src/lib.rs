//! Shared device-pairing + secure-channel protocol for the Baybo mobile
//! companion.
//!
//! This crate is the single source of truth for the cryptography the user's
//! baybo gateway (**A**) and the app's `baybo-mobile-core` (**P**) must agree on
//! byte-for-byte:
//!
//! - [`psk_pair`] — the Noise `XXpsk0` pairing handshake over the untrusted
//!   rendezvous. The operator-run `remote-host` (**C**) sees only a public
//!   rendezvous id and opaque Noise frames; the QR secret is the PSK, so a
//!   relay that lacks it is reduced from MITM to denial-of-service.
//! - [`kdf`] — HKDF expansion of the Noise handshake hash `h` into the labeled
//!   values both ends derive identically at pairing (the push key + the
//!   human-comparable confirm code).
//! - [`aead`] — the ChaCha20-Poly1305 lock-screen-preview framing. The layout
//!   (`ciphertext || 16-byte Poly1305 tag`, fresh random 12-byte nonce, empty
//!   AAD) matches Apple CryptoKit's `ChaChaPoly.SealedBox`, so the iOS
//!   Notification Service Extension decrypts a preview natively with no Rust.
//! - [`noise`] — the Noise (`snow`) IK/XX session both sides run for every
//!   direct or relayed connection after pairing.
//! - [`pairing`] — the wire messages swapped during the pairing handshake
//!   (the `DeviceHello` body, routing, push registration, confirm decision).
//! - [`fixtures`] — pinned cross-language test vectors the iOS NSE's Swift
//!   tests consume so the native CryptoKit path matches this crate exactly.
//!
//! No FFI and no host-only dependencies: it cross-compiles to
//! `aarch64-apple-ios` unchanged, so the gateway and the companion link the
//! same protocol.

pub mod aead;
pub mod error;
pub mod fixtures;
pub mod kdf;
pub mod noise;
pub mod pairing;
pub mod psk_pair;

pub use error::ProtoError;
