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
/// Output format: `nonce(12 bytes) || ciphertext || tag(16 bytes)`.
///
/// Unversioned on purpose. A leading marker would discriminate nothing — one
/// format exists — and a record's own bytes cannot disambiguate it anyway: the
/// first byte of a random nonce collides with any marker once in 256. If the
/// format ever does change, the discriminator has to come from outside the
/// record (a column, a per-store flag), not from a prefix.
///
/// `aad` is authenticated but **not** stored, so it must be something the
/// decrypting side already knows — the vault passes the entry name. Without
/// that binding one key encrypts every record interchangeably, and anyone able
/// to write the store can move a low-value entry's ciphertext onto a
/// high-value one and have it decrypt cleanly.
pub fn encrypt(plaintext: &[u8], key: &EncryptionKey, aad: &[u8]) -> crate::Result<Vec<u8>> {
    reject_empty_aad(aad)?;
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

    let mut output = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

/// Decrypt data produced by [`encrypt`], under the same `aad` it was written
/// with.
///
/// A record written under different associated data does not open, which is the
/// whole point of `aad` — one key encrypts every record, so without the binding
/// they would all be interchangeable.
pub fn decrypt(data: &[u8], key: &EncryptionKey, aad: &[u8]) -> crate::Result<Vec<u8>> {
    reject_empty_aad(aad)?;
    // Not merely a tidy early return: `open` splits at NONCE_LEN and panics on
    // anything shorter.
    if data.len() < NONCE_LEN + TAG_LEN {
        return Err(crate::SecurityError::Encryption(
            "too short to be an encrypted record".into(),
        ));
    }

    let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).map_err(|e| {
        crate::SecurityError::Encryption(format!("failed to create AES-256-GCM cipher: {e}"))
    })?;

    open(&cipher, data, aad)
        .map_err(|_| crate::SecurityError::Encryption("AES-256-GCM decryption failed".to_string()))
}

/// An empty `aad` is not a binding: AES-GCM treats "no associated data" and
/// "empty associated data" as the same input, so every record written that way
/// is interchangeable with every other — the exact property `aad` exists to
/// remove. Refusing it here makes an unbound record unrepresentable through this
/// module rather than merely unusual.
fn reject_empty_aad(aad: &[u8]) -> crate::Result<()> {
    if aad.is_empty() {
        return Err(crate::SecurityError::Encryption(
            "associated data must not be empty — a record has to be bound to an identity".into(),
        ));
    }
    Ok(())
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
        // nonce (12) + ciphertext (same len as plaintext) + tag (16)
        assert_eq!(encrypted.len(), NONCE_LEN + plaintext.len() + TAG_LEN);
        assert_ne!(&encrypted[NONCE_LEN..], plaintext.as_slice());

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

    /// A record encrypted with no associated data is identical in shape to a
    /// bound one, and AES-GCM cannot tell "absent" from "empty" — so an empty
    /// aad would open it and hand back the interchangeability. The API refuses
    /// that argument outright.
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
}
