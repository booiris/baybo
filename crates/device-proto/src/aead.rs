//! ChaCha20-Poly1305 framing for the encrypted lock-screen preview.
//!
//! A is the only encryptor (it owns the preview), C relays the ciphertext
//! blind, and the iOS Notification Service Extension decrypts it with native
//! CryptoKit. The wire layout is fixed so the Swift side matches byte-for-byte:
//!
//! - 32-byte key (the per-binding push key from [`crate::kdf`]),
//! - fresh random 12-byte nonce per message, carried alongside as `n`,
//! - ciphertext is `plaintext_ciphertext || 16-byte Poly1305 tag` (exactly
//!   what RustCrypto's [`Aead::encrypt`] returns and what CryptoKit's
//!   `ChaChaPoly.SealedBox { ciphertext, tag }` expects),
//! - empty / fixed AAD.
//!
//! [`crate::fixtures`] pins a known (key, nonce, plaintext, ciphertext) tuple
//! the NSE's Swift test decrypts to prove interop.

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::error::ProtoError;

/// Push-key / AEAD key length in bytes.
pub const KEY_LEN: usize = 32;
/// ChaCha20-Poly1305 nonce length in bytes.
pub const NONCE_LEN: usize = 12;
/// Poly1305 authentication tag length in bytes.
pub const TAG_LEN: usize = 16;

/// Encrypt `plaintext` under `key` with a fresh random nonce and empty AAD.
/// Returns `(nonce, ciphertext_with_tag)` — the two halves carried on the wire
/// as `n` and `enc`.
pub fn seal(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ProtoError> {
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher(key)
        .encrypt(&nonce, plaintext)
        .map_err(|_| ProtoError::Aead { stage: "seal" })?;
    Ok((nonce.to_vec(), ciphertext))
}

/// Deterministic variant of [`seal`] with a caller-supplied nonce. The nonce
/// MUST be unique per `key`; reusing one breaks confidentiality. Used by the
/// pinned cross-language fixtures and by callers that frame their own nonce.
pub fn seal_with_nonce(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, ProtoError> {
    cipher(key)
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|_| ProtoError::Aead { stage: "seal" })
}

/// Decrypt a `ciphertext_with_tag` produced by [`seal`] / [`seal_with_nonce`].
/// Fails (without distinguishing why) on a bad key, a wrong nonce, or any
/// tampering — the only safe response is to fall back to the placeholder.
pub fn open(key: &[u8; KEY_LEN], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, ProtoError> {
    if nonce.len() != NONCE_LEN {
        return Err(ProtoError::Aead { stage: "open" });
    }
    cipher(key)
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| ProtoError::Aead { stage: "open" })
}

fn cipher(key: &[u8; KEY_LEN]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(Key::from_slice(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_random_nonce() {
        let key = [7u8; KEY_LEN];
        let (nonce, ct) = seal(&key, b"the agent finished replying").unwrap();
        assert_eq!(nonce.len(), NONCE_LEN);
        // ciphertext = plaintext length + tag
        assert_eq!(ct.len(), b"the agent finished replying".len() + TAG_LEN);
        let pt = open(&key, &nonce, &ct).unwrap();
        assert_eq!(pt, b"the agent finished replying");
    }

    #[test]
    fn wrong_key_fails() {
        let (nonce, ct) = seal(&[1u8; KEY_LEN], b"secret preview").unwrap();
        assert!(open(&[2u8; KEY_LEN], &nonce, &ct).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = [3u8; KEY_LEN];
        let (nonce, mut ct) = seal(&key, b"secret preview").unwrap();
        ct[0] ^= 0xff;
        assert!(open(&key, &nonce, &ct).is_err());
    }

    #[test]
    fn wrong_nonce_length_rejected() {
        let key = [4u8; KEY_LEN];
        let (_n, ct) = seal(&key, b"x").unwrap();
        assert!(open(&key, &[0u8; NONCE_LEN - 1], &ct).is_err());
    }

    #[test]
    fn deterministic_nonce_is_reproducible() {
        let key = [9u8; KEY_LEN];
        let nonce = [1u8; NONCE_LEN];
        let a = seal_with_nonce(&key, &nonce, b"same input").unwrap();
        let b = seal_with_nonce(&key, &nonce, b"same input").unwrap();
        assert_eq!(a, b, "fixed key+nonce+plaintext must be deterministic");
        assert_eq!(open(&key, &nonce, &a).unwrap(), b"same input");
    }
}
