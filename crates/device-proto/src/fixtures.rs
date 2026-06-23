//! Pinned cross-language test vector for the lock-screen-preview AEAD.
//!
//! The iOS Notification Service Extension decrypts previews with native
//! CryptoKit (`ChaChaPoly`), linking no Rust. To prove the two
//! implementations agree byte-for-byte, this module pins a known
//! `(KEY, NONCE, PLAINTEXT) → CIPHERTEXT` tuple produced by [`crate::aead`].
//! The NSE's Swift test builds `ChaChaPoly.SealedBox` from [`KEY`] / [`NONCE`]
//! / [`ciphertext_bytes`] and asserts the opened plaintext equals
//! [`PLAINTEXT`]; this crate's own test asserts the same bytes are still
//! reproduced, so a drift on either side is a failing test.

use crate::aead::{KEY_LEN, NONCE_LEN};

/// Fixed 32-byte key for the fixture (0x00..=0x1f).
pub const KEY: [u8; KEY_LEN] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];

/// Fixed 12-byte nonce for the fixture (0xa0..=0xab).
pub const NONCE: [u8; NONCE_LEN] = [
    0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab,
];

/// The preview plaintext — the shape A encrypts and the NSE rewrites into
/// `title` / `body`.
pub const PLAINTEXT: &[u8] = br#"{"title":"Baybo","body":"The agent finished replying."}"#;

/// `ChaCha20-Poly1305(KEY, NONCE, PLAINTEXT, aad=∅)` = `ciphertext || tag`,
/// hex-encoded. Pinned so the NSE's CryptoKit decrypt is validated against an
/// exact byte sequence.
pub const CIPHERTEXT_HEX: &str = "77890c36398aa78f9a2db2618e9bdfd7bf3cbcdb3a485a81e0b0be911005a66218a070e3c0a08ce2a3a8d5cf7821143f23aa2d162b71a68f69fd2b8d48cff9e1cc8670a8738b";

/// Decode [`CIPHERTEXT_HEX`] into bytes (`ciphertext || tag`).
pub fn ciphertext_bytes() -> Vec<u8> {
    hex::decode(CIPHERTEXT_HEX).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aead::{open, seal_with_nonce};

    #[test]
    fn pinned_vector_is_reproduced() {
        let ct = seal_with_nonce(&KEY, &NONCE, PLAINTEXT).unwrap();
        assert_eq!(
            hex::encode(&ct),
            CIPHERTEXT_HEX,
            "AEAD output drifted — update CIPHERTEXT_HEX *and* the Swift NSE \
             fixture together, never one alone"
        );
    }

    #[test]
    fn pinned_vector_decrypts() {
        let pt = open(&KEY, &NONCE, &ciphertext_bytes()).unwrap();
        assert_eq!(pt, PLAINTEXT);
    }
}
