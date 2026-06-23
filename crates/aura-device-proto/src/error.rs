//! Error surface for the device-pairing protocol.

use thiserror::Error;

/// Everything that can go wrong inside `aura-device-proto`. Crypto failures
/// are deliberately coarse — an attacker learns nothing from a more precise
/// reason, and the only correct response to any of them is to abort the
/// pairing / drop the message.
#[derive(Debug, Error)]
pub enum ProtoError {
    /// AEAD seal/open failed (bad key, tampered ciphertext, wrong nonce
    /// length). `stage` names the operation, never the secret.
    #[error("aead {stage} failed")]
    Aead { stage: &'static str },

    /// SPAKE2 start/finish failed — almost always a code mismatch or a
    /// corrupted/forged peer message.
    #[error("pake: {0}")]
    Pake(String),

    /// Underlying Noise handshake / transport error.
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),

    /// HKDF expand rejected the requested output length (impossible for our
    /// fixed 32-byte subkeys, but surfaced rather than panicked).
    #[error("hkdf expand rejected output length")]
    Hkdf,

    /// MessagePack encode/decode of a pairing message failed.
    #[error("codec: {0}")]
    Codec(String),

    /// A byte slice handed in where a fixed-length key was expected had the
    /// wrong length.
    #[error("invalid key length: expected {expected}, got {got}")]
    KeyLen { expected: usize, got: usize },
}
