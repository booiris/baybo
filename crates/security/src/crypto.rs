//! AES-256-GCM authenticated encryption/decryption.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use rand::Rng;

/// AES-256-GCM requires exactly 32 bytes for the key.
const AES256_KEY_LEN: usize = 32;

/// AES-GCM standard nonce size: 12 bytes.
const NONCE_LEN: usize = 12;

/// AES-GCM authentication tag size: 16 bytes, appended to the ciphertext by the
/// `aes-gcm` crate.
const TAG_LEN: usize = 16;

/// Leading byte of every record: `0x02 || nonce || ct || tag`.
///
/// Discriminates nothing — one format exists, and rotation re-keys every record
/// in one transaction rather than tagging each with the key that wrote it, so
/// nothing reads this to make a decision.
///
/// It stays because removing it costs more than keeping it. Every record on disk
/// begins with this byte, so dropping it is a full re-encryption pass over the
/// vault — the same migration the tree deliberately keeps no tooling for. And
/// the bounds check it anchors in [`decrypt`] is load-bearing on its own: `open`
/// splits at [`NONCE_LEN`] and would panic on a shorter input.
const FORMAT_V2: u8 = 0x02;

/// Encryption key wrapper holding raw key bytes suitable for AES-256-GCM.
#[derive(Clone)]
pub struct EncryptionKey {
    key: Vec<u8>,
}

impl EncryptionKey {
    /// Create a new encryption key from raw bytes.
    ///
    /// Returns an error if the key is not exactly 32 bytes.
    pub fn new(key: Vec<u8>) -> crate::Result<Self> {
        if key.len() != AES256_KEY_LEN {
            return Err(crate::SecurityError::Encryption(format!(
                "encryption key must be exactly {AES256_KEY_LEN} bytes, got {}",
                key.len()
            )));
        }
        Ok(Self { key })
    }

    /// Return a reference to the raw key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.key
    }
}

/// Encrypt `plaintext` using AES-256-GCM, binding it to `aad`.
///
/// Output format: `0x02 || nonce(12 bytes) || ciphertext || tag(16 bytes)`.
///
/// `aad` is authenticated but **not** stored, so it must be something the
/// decrypting side already knows — the vault passes the entry name. Without
/// that binding one key encrypts every record interchangeably, and anyone able
/// to write the store can move a low-value entry's ciphertext onto a
/// high-value one and have it decrypt cleanly.
pub fn encrypt(plaintext: &[u8], key: &EncryptionKey, aad: &[u8]) -> crate::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).map_err(|e| {
        crate::SecurityError::Encryption(format!("failed to create AES-256-GCM cipher: {e}"))
    })?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            nonce,
            aes_gcm::aead::Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|e| {
            crate::SecurityError::Encryption(format!("AES-256-GCM encryption failed: {e}"))
        })?;

    let mut output = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
    output.push(FORMAT_V2);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

/// Decrypt data produced by [`encrypt`], under the same `aad` it was written
/// with.
///
/// Anything else is rejected, including a record with the marker stripped: an
/// unmarked `nonce || ct || tag` record carries no binding and so opens under
/// any identity. Falling back to it would hand that property back to anyone who
/// can write the store.
pub fn decrypt(data: &[u8], key: &EncryptionKey, aad: &[u8]) -> crate::Result<Vec<u8>> {
    if data.first() != Some(&FORMAT_V2) || data.len() < 1 + NONCE_LEN + TAG_LEN {
        return Err(crate::SecurityError::Encryption(
            "not a v2 encrypted record".into(),
        ));
    }

    let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).map_err(|e| {
        crate::SecurityError::Encryption(format!("failed to create AES-256-GCM cipher: {e}"))
    })?;

    open(&cipher, &data[1..], aad)
        .map_err(|_| crate::SecurityError::Encryption("AES-256-GCM decryption failed".to_string()))
}

/// Split `nonce || ct || tag` and open it under `aad`.
fn open(cipher: &Aes256Gcm, body: &[u8], aad: &[u8]) -> Result<Vec<u8>, aes_gcm::Error> {
    let (nonce_bytes, ciphertext) = body.split_at(NONCE_LEN);
    cipher.decrypt(
        Nonce::from_slice(nonce_bytes),
        aes_gcm::aead::Payload {
            msg: ciphertext,
            aad,
        },
    )
}

impl EncryptionKey {
    /// Mint a fresh 32-byte key from the OS RNG. Used by `baybo setup`
    /// for first-run key generation; tests use it for fixtures.
    pub fn generate() -> Self {
        use rand::RngExt;
        let mut key = vec![0u8; AES256_KEY_LEN];
        rand::rng().fill(key.as_mut_slice());
        Self { key }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = EncryptionKey::generate();
        let plaintext = b"hello, world! this is a secret.";

        let encrypted = encrypt(plaintext, &key, b"entry-name").unwrap();
        // Version (1) + nonce (12) + ciphertext (same len as plaintext) + tag (16)
        assert_eq!(encrypted.len(), 1 + NONCE_LEN + plaintext.len() + TAG_LEN);
        assert_eq!(encrypted[0], FORMAT_V2);
        // Ciphertext portion should differ from plaintext.
        assert_ne!(&encrypted[1 + NONCE_LEN..], plaintext.as_slice());

        let decrypted = decrypt(&encrypted, &key, b"entry-name").unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn different_nonces_produce_different_ciphertext() {
        let key = EncryptionKey::generate();
        let plaintext = b"same plaintext for both encryptions";

        let encrypted1 = encrypt(plaintext, &key, b"n").unwrap();
        let encrypted2 = encrypt(plaintext, &key, b"n").unwrap();

        // Random nonces should differ, making ciphertext differ.
        assert_ne!(encrypted1, encrypted2);

        // Both should decrypt to the same plaintext.
        assert_eq!(decrypt(&encrypted1, &key, b"n").unwrap(), plaintext);
        assert_eq!(decrypt(&encrypted2, &key, b"n").unwrap(), plaintext);
    }

    #[test]
    fn empty_data_errors() {
        let key = EncryptionKey::generate();
        assert!(decrypt(&[], &key, b"n").is_err());
    }

    #[test]
    fn truncated_data_errors() {
        let key = EncryptionKey::generate();
        // Only nonce bytes, no ciphertext+tag — decryption should fail.
        assert!(decrypt(&[0u8; NONCE_LEN], &key, b"n").is_err());
    }

    #[test]
    fn wrong_key_size_errors() {
        assert!(EncryptionKey::new(b"too-short".to_vec()).is_err());
        assert!(EncryptionKey::new(vec![0u8; 64]).is_err());
    }

    #[test]
    fn correct_key_size_succeeds() {
        assert!(EncryptionKey::new(vec![0u8; 32]).is_ok());
    }

    #[test]
    fn wrong_key_fails_decryption() {
        let key1 = EncryptionKey::generate();
        let key2 = EncryptionKey::generate();
        let plaintext = b"secret data";

        let encrypted = encrypt(plaintext, &key1, b"n").unwrap();
        assert!(decrypt(&encrypted, &key2, b"n").is_err());
    }

    /// Turns "can write the store" into "can choose what a privileged entry
    /// contains" if the binding is ever dropped.
    #[test]
    fn ciphertext_cannot_be_moved_between_entries() {
        let key = EncryptionKey::generate();
        let stolen = encrypt(b"attacker-chosen value", &key, b"llm.entry.cheap.api_key").unwrap();

        let err = decrypt(&stolen, &key, b"gateway.admin_token");
        assert!(
            err.is_err(),
            "a ciphertext must not decrypt under a different entry name"
        );

        // Still fine where it belongs.
        assert_eq!(
            decrypt(&stolen, &key, b"llm.entry.cheap.api_key").unwrap(),
            b"attacker-chosen value"
        );
    }

    /// An unmarked record is valid under every identity, so accepting one would
    /// return the interchangeability the binding removes.
    #[test]
    fn unbound_records_are_rejected() {
        let key = EncryptionKey::generate();

        let unbound = {
            let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).unwrap();
            let mut nonce_bytes = [0u8; NONCE_LEN];
            rand::rng().fill_bytes(&mut nonce_bytes);
            let ct = cipher
                .encrypt(Nonce::from_slice(&nonce_bytes), b"unbound value".as_slice())
                .unwrap();
            let mut out = nonce_bytes.to_vec();
            out.extend_from_slice(&ct);
            out
        };

        assert!(decrypt(&unbound, &key, b"any-name").is_err());
        assert!(
            decrypt(&unbound, &key, b"").is_err(),
            "an empty aad must not be a backdoor into the unbound layout"
        );
    }

    /// The downgrade attempt in its most direct form.
    #[test]
    fn stripping_the_version_byte_does_not_downgrade_a_record() {
        let key = EncryptionKey::generate();
        let bound = encrypt(b"v", &key, b"gateway.admin_token").unwrap();

        let stripped = &bound[1..];
        assert!(decrypt(stripped, &key, b"gateway.admin_token").is_err());
        assert!(decrypt(stripped, &key, b"").is_err());
    }
}
