//! HKDF-SHA256 derivation of the pairing subkeys from the Noise handshake hash.
//!
//! After the [`crate::psk_pair`] XXpsk0 handshake both ends hold the same
//! handshake hash `h` ([`PskTransport::handshake_hash`]). `h` commits to the
//! whole transcript — the prologue, both ephemerals, the PSK mix, and **both
//! statics** — and, because the PSK is folded in, is not computable by a relay
//! that lacks the secret. We expand it with HKDF into independent, labeled
//! values:
//!
//! - the **push key** (the long-lived AEAD key the iOS NSE decrypts previews
//!   with), and
//! - the human-comparable **confirm code** (channel binding for the two-sided
//!   confirm).
//!
//! Both sides run this identically — no key material crosses the wire.
//!
//! [`PskTransport::handshake_hash`]: crate::psk_pair::PskTransport::handshake_hash

use hkdf::Hkdf;
use sha2::Sha256;

use crate::aead::KEY_LEN;
use crate::error::ProtoError;

/// HKDF `info` label for the push key (the AEAD key the NSE decrypts previews
/// with). Versioned so a future rotation scheme can bump it without ambiguity.
pub const PUSH_KEY_INFO: &[u8] = b"baybo/device/push-key/v1";

/// HKDF `info` label for the human-comparable pairing confirmation code.
pub const CONFIRM_CODE_INFO: &[u8] = b"baybo/device/pair-confirm/v1";

/// Number of decimal digits in the displayed confirmation code.
pub const CONFIRM_CODE_DIGITS: u32 = 6;

/// Derive the long-lived per-binding push key from the handshake hash `h`. The
/// gateway stores the result in its `SecretVault`; the app stores it in the
/// shared Keychain for the NSE. Both derive it identically from the same `h`.
pub fn derive_push_key(h: &[u8]) -> Result<[u8; KEY_LEN], ProtoError> {
    expand(h, PUSH_KEY_INFO)
}

/// Derive the human-comparable confirmation code (a zero-padded
/// [`CONFIRM_CODE_DIGITS`]-digit decimal string) from the handshake hash `h`.
///
/// Both ends compute it identically and display it; the phone user and the
/// operator confirm the two codes match. Because it folds from `h` — which binds
/// both statics by construction (XX) — a match attests the real device
/// identities of a single live session, and is **not** grindable or
/// precomputable the way the old SPAKE2-`K` code was.
pub fn derive_confirm_code(h: &[u8]) -> Result<String, ProtoError> {
    let okm = expand(h, CONFIRM_CODE_INFO)?;
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
    fn push_key_is_stable_and_h_dependent() {
        let h = b"shared noise handshake hash bytes";
        assert_eq!(derive_push_key(h).unwrap(), derive_push_key(h).unwrap());
        assert_ne!(
            derive_push_key(h).unwrap(),
            derive_push_key(b"different handshake hash").unwrap(),
        );
    }

    #[test]
    fn push_key_and_confirm_are_domain_separated() {
        let h = b"some handshake hash";
        let push = derive_push_key(h).unwrap();
        let confirm = expand(h, CONFIRM_CODE_INFO).unwrap();
        assert_ne!(
            push.as_slice(),
            confirm.as_slice(),
            "push and confirm derive under distinct info labels"
        );
    }

    #[test]
    fn confirm_code_is_stable_padded_and_h_dependent() {
        let code = derive_confirm_code(b"shared noise handshake hash").unwrap();
        assert_eq!(
            code.len(),
            CONFIRM_CODE_DIGITS as usize,
            "zero-padded width"
        );
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        // Both ends derive the same code from the same h...
        assert_eq!(
            code,
            derive_confirm_code(b"shared noise handshake hash").unwrap()
        );
        // ...and a different h (e.g. a MITM with a different session) almost
        // certainly shows a different number.
        assert_ne!(
            code,
            derive_confirm_code(b"different handshake hash").unwrap()
        );
    }
}
