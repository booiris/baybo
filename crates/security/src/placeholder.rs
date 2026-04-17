use std::sync::OnceLock;

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use regex::Regex;
use sha2::Sha256;

use crate::crypto::EncryptionKey;

const HKDF_INFO: &[u8] = b"aura-placeholder-v1";
const PLACEHOLDER_HEX_LEN: usize = 24;

/// Deterministically mints `{{SECRET_<hex>}}` placeholders from raw secret
/// bytes using HMAC-SHA256 with a subkey derived from the master encryption
/// key via HKDF-Expand.
///
/// Same secret bytes always produce the same placeholder within a process,
/// which bounds vault growth to one entry per unique secret.
pub struct PlaceholderMinter {
    hmac_key: [u8; 32],
}

impl PlaceholderMinter {
    /// Derive an HMAC subkey from the master encryption key using HKDF-Expand.
    /// Domain-separated from AES-GCM usage via `HKDF_INFO`. `EncryptionKey`
    /// is always 32 bytes (AES-256), which satisfies HKDF-SHA256's PRK size
    /// requirement (≥ 32 bytes).
    pub fn from_master_key(key: &EncryptionKey) -> Self {
        let hk = Hkdf::<Sha256>::from_prk(key.as_bytes())
            .expect("32-byte EncryptionKey is a valid HKDF-SHA256 PRK");
        let mut okm = [0u8; 32];
        hk.expand(HKDF_INFO, &mut okm)
            .expect("HKDF expand of 32 bytes is infallible");
        Self { hmac_key: okm }
    }

    /// Mint a deterministic placeholder for the given secret.
    pub fn mint(&self, secret: &[u8]) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.hmac_key)
            .expect("HMAC-SHA256 accepts a 32-byte key");
        mac.update(secret);
        let tag = mac.finalize().into_bytes();
        let full = hex::encode(tag);
        format!("{{{{SECRET_{}}}}}", &full[..PLACEHOLDER_HEX_LEN])
    }

    /// Regex matching any well-formed placeholder emitted by `mint`, plus
    /// compatible widths 16–32 hex chars for forward compatibility.
    pub fn placeholder_regex() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| {
            Regex::new(r"\{\{SECRET_[0-9a-f]{16,32}\}\}")
                .expect("static placeholder regex compiles")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key() -> EncryptionKey {
        EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap()
    }

    #[test]
    fn same_secret_same_placeholder() {
        let minter = PlaceholderMinter::from_master_key(&make_key());
        let a = minter.mint(b"AKIAIOSFODNN7EXAMPLE");
        let b = minter.mint(b"AKIAIOSFODNN7EXAMPLE");
        assert_eq!(a, b);
    }

    #[test]
    fn different_secrets_different_placeholders() {
        let minter = PlaceholderMinter::from_master_key(&make_key());
        assert_ne!(minter.mint(b"secret-a"), minter.mint(b"secret-b"));
    }

    #[test]
    fn placeholder_format() {
        let minter = PlaceholderMinter::from_master_key(&make_key());
        let ph = minter.mint(b"something");
        assert!(ph.starts_with("{{SECRET_"));
        assert!(ph.ends_with("}}"));
        // "{{SECRET_" (9) + 24 hex + "}}" (2)
        assert_eq!(ph.len(), 9 + PLACEHOLDER_HEX_LEN + 2);
    }

    #[test]
    fn regex_matches_minted_placeholders() {
        let minter = PlaceholderMinter::from_master_key(&make_key());
        let ph = minter.mint(b"x");
        assert!(PlaceholderMinter::placeholder_regex().is_match(&ph));
    }

    #[test]
    fn different_master_keys_different_placeholders() {
        let k1 = EncryptionKey::new(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec()).unwrap();
        let k2 = EncryptionKey::new(b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_vec()).unwrap();
        let m1 = PlaceholderMinter::from_master_key(&k1);
        let m2 = PlaceholderMinter::from_master_key(&k2);
        assert_ne!(m1.mint(b"secret"), m2.mint(b"secret"));
    }
}
