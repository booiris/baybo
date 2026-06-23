//! HKDF-SHA256 expansion of the SPAKE2 master secret into the pairing
//! subkeys.
//!
//! After SPAKE2 both ends hold the same master secret `K`. It is never used
//! directly: HKDF-Expand splits it into independent, labeled subkeys so a
//! compromise of one (the hot, unlocked-class push key the NSE reads) cannot
//! reveal the other (the K-channel key). The SPAKE2 output is already a
//! uniformly-random strong key, so the salt is empty and separation is purely
//! by `info` label. Both sides run this identically — no key material crosses
//! the wire.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::aead::KEY_LEN;
use crate::error::ProtoError;

/// HKDF `info` label for the push key (the AEAD key the NSE decrypts previews
/// with). Versioned so a future rotation scheme can bump it without ambiguity.
pub const PUSH_KEY_INFO: &[u8] = b"aura/device/push-key/v1";

/// HKDF `info` label for the one-time SPAKE2 K-channel key (protects the
/// static-key exchange + push registration during pairing).
pub const CHANNEL_KEY_INFO: &[u8] = b"aura/device/pair-channel/v1";

/// The subkeys both ends derive from the SPAKE2 master secret at pairing.
#[derive(Clone)]
pub struct PairKeys {
    /// AEAD key for the pairing K-channel messages ([`crate::pairing`]).
    pub channel_key: [u8; KEY_LEN],
    /// The long-lived per-binding push key A encrypts previews with and the
    /// NSE decrypts them with.
    pub push_key: [u8; KEY_LEN],
}

/// Expand the SPAKE2 master secret `k` into the labeled subkeys.
pub fn derive_pair_keys(k: &[u8]) -> Result<PairKeys, ProtoError> {
    Ok(PairKeys {
        channel_key: expand(k, CHANNEL_KEY_INFO)?,
        push_key: expand(k, PUSH_KEY_INFO)?,
    })
}

/// Derive just the push key (the gateway re-derives this at pairing to store
/// in its `SecretVault`; the app stores it in the shared Keychain).
pub fn derive_push_key(k: &[u8]) -> Result<[u8; KEY_LEN], ProtoError> {
    expand(k, PUSH_KEY_INFO)
}

fn expand(ikm: &[u8], info: &[u8]) -> Result<[u8; KEY_LEN], ProtoError> {
    let hk = Hkdf::<Sha256>::new(None, ikm);
    let mut okm = [0u8; KEY_LEN];
    hk.expand(info, &mut okm).map_err(|_| ProtoError::Hkdf)?;
    Ok(okm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_ends_derive_identical_subkeys() {
        let k = b"shared spake2 master secret bytes";
        let a = derive_pair_keys(k).unwrap();
        let b = derive_pair_keys(k).unwrap();
        assert_eq!(a.channel_key, b.channel_key);
        assert_eq!(a.push_key, b.push_key);
    }

    #[test]
    fn subkeys_are_domain_separated() {
        let keys = derive_pair_keys(b"master").unwrap();
        assert_ne!(
            keys.channel_key, keys.push_key,
            "channel and push keys must differ under distinct info labels"
        );
    }

    #[test]
    fn different_master_yields_different_push_key() {
        assert_ne!(
            derive_push_key(b"master-one").unwrap(),
            derive_push_key(b"master-two").unwrap(),
        );
    }

    #[test]
    fn derive_push_key_matches_pair_keys() {
        let k = b"master";
        assert_eq!(
            derive_push_key(k).unwrap(),
            derive_pair_keys(k).unwrap().push_key,
        );
    }
}
