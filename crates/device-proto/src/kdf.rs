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
pub const PUSH_KEY_INFO: &[u8] = b"baybo/device/push-key/v1";

/// HKDF `info` label for the one-time SPAKE2 K-channel key (protects the
/// static-key exchange + push registration during pairing).
pub const CHANNEL_KEY_INFO: &[u8] = b"baybo/device/pair-channel/v1";

/// HKDF `info` label for the human-comparable pairing confirmation code.
pub const CONFIRM_CODE_INFO: &[u8] = b"baybo/device/pair-confirm/v1";

/// Number of decimal digits in the displayed confirmation code.
pub const CONFIRM_CODE_DIGITS: u32 = 6;

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

/// Derive the human-comparable confirmation code (a zero-padded
/// [`CONFIRM_CODE_DIGITS`]-digit decimal string) from the SPAKE2 master secret.
///
/// Both ends compute it identically and display it; the operator and the phone
/// user confirm the two codes match. Because it is folded from `K`, a match
/// proves both ends derived the same secret — a Bluetooth-style numeric
/// comparison layered on top of the QR's out-of-band channel.
pub fn derive_confirm_code(k: &[u8]) -> Result<String, ProtoError> {
    let okm = expand(k, CONFIRM_CODE_INFO)?;
    // Fold the first 8 bytes (big-endian) into a u64, reduce mod 10^digits.
    let folded = okm
        .iter()
        .take(8)
        .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
    let modulo = 10u64.pow(CONFIRM_CODE_DIGITS);
    Ok(format!(
        "{:0width$}",
        folded % modulo,
        width = CONFIRM_CODE_DIGITS as usize
    ))
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

    #[test]
    fn confirm_code_is_stable_padded_and_secret_dependent() {
        let code = derive_confirm_code(b"shared spake2 master secret").unwrap();
        assert_eq!(code.len(), CONFIRM_CODE_DIGITS as usize, "zero-padded width");
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        // Both ends derive the same code from the same K...
        assert_eq!(code, derive_confirm_code(b"shared spake2 master secret").unwrap());
        // ...and a different K (e.g. a MITM with a different secret) almost
        // certainly shows a different number.
        assert_ne!(code, derive_confirm_code(b"different master secret").unwrap());
    }
}
