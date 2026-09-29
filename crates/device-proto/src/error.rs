//! Error surface for the device-pairing protocol.

use thiserror::Error;

/// Everything that can go wrong inside `device-proto`. Crypto failures
/// are deliberately coarse — an attacker learns nothing from a more precise
/// reason, and the only correct response to any of them is to abort the
/// pairing / drop the message.
#[derive(Debug, Error)]
pub enum ProtoError {
    /// AEAD seal/open failed (bad key, tampered ciphertext, wrong nonce
    /// length). `stage` names the operation, never the secret.
    #[error("aead {stage} failed")]
    Aead { stage: &'static str },

    /// A pairing-handshake setup error that isn't a `snow` error proper — a
    /// bad Noise pattern string, or a malformed prologue/secret. The live
    /// XXpsk0 failures (wrong PSK, tampered frame) surface as [`Self::Noise`].
    #[error("handshake: {0}")]
    Handshake(String),

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

    /// A content frame's chunking length prefix exceeded the cap — a garbled or
    /// hostile length, refused before allocating.
    #[error("content frame too large: {len} bytes")]
    FrameTooLarge { len: usize },

    /// An Ed25519 push-delegation key/signature was malformed or failed to
    /// verify. `stage` names the operation, never the secret — verification is
    /// constant in what it reveals.
    #[error("signature {stage} failed")]
    Signature { stage: &'static str },

    /// The peer's X25519 static key is a low-order point: the static-static DH
    /// output is all zero, so no candidate keys are derived.
    #[error("x25519 peer key yields an all-zero shared secret")]
    WeakPeerKey,

    /// A sealed candidate set was refused whole, at seal or at open.
    #[error("sealed candidates: {0}")]
    Candidates(#[from] CandidateRejection),
}

/// Why a sealed candidate set was refused. No variant carries a candidate, the
/// token or key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CandidateRejection {
    #[error("sealed set exceeds the size cap")]
    Oversized,
    #[error("sealed set is not valid base64")]
    Encoding,
    #[error("nonce is {got} bytes, expected {expected}")]
    NonceLength { expected: usize, got: usize },
    #[error("ciphertext is {got} bytes, expected {expected}")]
    CiphertextLength { expected: usize, got: usize },
    #[error("body is {len} bytes, the plaintext holds {capacity}")]
    BodyTooLarge { len: usize, capacity: usize },
    #[error("declared body length does not frame exactly one body")]
    BodyLength,
    #[error("plaintext padding is not zero")]
    Padding,
    #[error("candidate set version {got} is not supported")]
    Version { got: u8 },
    #[error("{len} {list} candidates exceed the cap of {max}")]
    Count {
        list: &'static str,
        len: usize,
        max: usize,
    },
    #[error("answer does not echo the offer id")]
    OfferIdMismatch,
}
