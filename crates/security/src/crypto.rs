//! AES-256-GCM authenticated encryption/decryption.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use rand::Rng;

/// AES-256-GCM requires exactly 32 bytes for the key.
const AES256_KEY_LEN: usize = 32;

/// AES-GCM standard nonce size: 12 bytes.
const NONCE_LEN: usize = 12;

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

/// Encrypt `plaintext` using AES-256-GCM.
///
/// Output format: `nonce(12 bytes) || ciphertext || tag`
/// (the `aes-gcm` crate appends the 16-byte authentication tag to the ciphertext).
pub fn encrypt(plaintext: &[u8], key: &EncryptionKey) -> crate::Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).map_err(|e| {
        crate::SecurityError::Encryption(format!("failed to create AES-256-GCM cipher: {e}"))
    })?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher.encrypt(nonce, plaintext).map_err(|e| {
        crate::SecurityError::Encryption(format!("AES-256-GCM encryption failed: {e}"))
    })?;

    let mut output = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

/// Decrypt data produced by [`encrypt`].
///
/// Expects the format: `nonce(12 bytes) || ciphertext || tag`.
pub fn decrypt(data: &[u8], key: &EncryptionKey) -> crate::Result<Vec<u8>> {
    if data.len() < NONCE_LEN {
        return Err(crate::SecurityError::Encryption(
            "encrypted data is too short to contain nonce".into(),
        ));
    }

    let (nonce_bytes, ciphertext) = data.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);

    let cipher = Aes256Gcm::new_from_slice(key.as_bytes()).map_err(|e| {
        crate::SecurityError::Encryption(format!("failed to create AES-256-GCM cipher: {e}"))
    })?;

    cipher.decrypt(nonce, ciphertext).map_err(|e| {
        crate::SecurityError::Encryption(format!("AES-256-GCM decryption failed: {e}"))
    })
}

impl EncryptionKey {
    /// Mint a fresh 32-byte key from the OS RNG. Used by `aura setup`
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

        let encrypted = encrypt(plaintext, &key).unwrap();
        // Nonce (12) + ciphertext (same len as plaintext) + tag (16)
        assert_eq!(encrypted.len(), NONCE_LEN + plaintext.len() + 16);
        // Ciphertext portion should differ from plaintext.
        assert_ne!(&encrypted[NONCE_LEN..], plaintext.as_slice());

        let decrypted = decrypt(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn different_nonces_produce_different_ciphertext() {
        let key = EncryptionKey::generate();
        let plaintext = b"same plaintext for both encryptions";

        let encrypted1 = encrypt(plaintext, &key).unwrap();
        let encrypted2 = encrypt(plaintext, &key).unwrap();

        // Random nonces should differ, making ciphertext differ.
        assert_ne!(encrypted1, encrypted2);

        // Both should decrypt to the same plaintext.
        assert_eq!(decrypt(&encrypted1, &key).unwrap(), plaintext);
        assert_eq!(decrypt(&encrypted2, &key).unwrap(), plaintext);
    }

    #[test]
    fn empty_data_errors() {
        let key = EncryptionKey::generate();
        assert!(decrypt(&[], &key).is_err());
    }

    #[test]
    fn truncated_data_errors() {
        let key = EncryptionKey::generate();
        // Only nonce bytes, no ciphertext+tag — decryption should fail.
        assert!(decrypt(&[0u8; NONCE_LEN], &key).is_err());
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

        let encrypted = encrypt(plaintext, &key1).unwrap();
        assert!(decrypt(&encrypted, &key2).is_err());
    }
}
