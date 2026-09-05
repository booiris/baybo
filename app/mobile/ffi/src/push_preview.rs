//! Decrypt a lock-screen push preview in the core.
//!
//! iOS does not come through here: its Notification Service Extension runs in a
//! separate process that links no Rust, so it opens the preview with CryptoKit
//! and is held to this crate's bytes by the pinned vector in
//! `device_proto::fixtures`. Android has no extension process — FCM delivers a
//! data-only message to the app process — so its messaging service calls this
//! and the push key never has to cross into Kotlin.
//!
//! The wire shape is the gateway's (`crates/gateway/src/push`): `enc` is
//! base64-STANDARD `ciphertext || tag`, `n` is the base64-STANDARD 12-byte
//! nonce, `bid` names the device whose push key opens it, and the plaintext is
//! `{"title","body","session_id"?,"badge"?}`.

use base64::Engine;
use serde::Deserialize;

use crate::api::PushPreview;
use crate::keychain;

/// The plaintext the gateway seals. `badge` is deliberately lenient — see
/// [`tolerant_badge`].
#[derive(Deserialize)]
struct Sealed {
    title: String,
    body: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default, deserialize_with = "tolerant_badge")]
    badge: Option<u32>,
}

/// A `badge` of an unexpected JSON type costs only the badge.
///
/// The all-or-nothing decode is the wrong trade here and the iOS NSE makes the
/// same one for the same reason: a shipped client cannot be rolled back to match
/// a gateway that starts emitting something unexpected, and losing the envelope
/// also loses `session_id` — which is what makes tapping the notification open
/// the right conversation. A wrong badge is cosmetic; a wrong tap target is not.
fn tolerant_badge<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_u64().and_then(|n| u32::try_from(n).ok()))
}

/// Open one push preview.
///
/// `Ok(None)` means "this device cannot render a preview for that payload" —
/// no push key stored for `bid`, a payload that does not decode, a nonce of the
/// wrong length, or a tag that does not verify. Every one of those is a
/// legitimate reason for the shell to post its generic fallback notification,
/// and none of them is worth distinguishing at the call site. `Err` is reserved
/// for the store itself failing, which the shell must not treat as "no key".
pub(crate) fn decrypt_push_preview(
    bid: &str,
    enc_b64: &str,
    nonce_b64: &str,
) -> Result<Option<PushPreview>, String> {
    // A read failure here is NOT absence: reporting it as `Ok(None)` would make
    // a locked keystore look like an unpaired device. Absence is `Ok(None)`.
    let Some(key) = keychain::read_push_key(bid)? else {
        return Ok(None);
    };

    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(ciphertext), Ok(nonce)) = (b64.decode(enc_b64), b64.decode(nonce_b64)) else {
        return Ok(None);
    };
    if nonce.len() != device_proto::aead::NONCE_LEN
        || ciphertext.len() < device_proto::aead::TAG_LEN
    {
        return Ok(None);
    }

    let Ok(plaintext) = device_proto::aead::open(&key, &nonce, &ciphertext) else {
        return Ok(None);
    };
    let Ok(sealed) = serde_json::from_slice::<Sealed>(&plaintext) else {
        return Ok(None);
    };

    Ok(Some(PushPreview {
        title: sealed.title,
        body: sealed.body,
        session_id: sealed.session_id,
        badge: sealed.badge,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure_store::test_support::shared_memory_store;

    const BID: &str = "device-abc";

    fn seal(key: &[u8; device_proto::aead::KEY_LEN], plaintext: &[u8]) -> (String, String) {
        let (nonce, ciphertext) =
            device_proto::aead::seal(key, plaintext).expect("seal the fixture");
        let b64 = base64::engine::general_purpose::STANDARD;
        (b64.encode(ciphertext), b64.encode(nonce))
    }

    /// The pinned cross-language vector: the same bytes the iOS NSE's CryptoKit
    /// test opens. If this and that test ever disagree, one of the two
    /// implementations has drifted.
    #[test]
    fn the_pinned_fixture_opens_to_the_pinned_plaintext() {
        shared_memory_store();
        let bid = "device-fixture";
        keychain::store_push_key(bid, &device_proto::fixtures::KEY).expect("store the fixture key");
        let b64 = base64::engine::general_purpose::STANDARD;
        let preview = decrypt_push_preview(
            bid,
            &b64.encode(device_proto::fixtures::ciphertext_bytes()),
            &b64.encode(device_proto::fixtures::NONCE),
        )
        .expect("no store failure")
        .expect("the fixture decrypts");
        assert_eq!(preview.title, "Baybo");
        assert_eq!(preview.body, "The agent finished replying.");
        assert_eq!(preview.session_id, None);
        assert_eq!(preview.badge, None);
    }

    #[test]
    fn session_id_and_badge_round_trip() {
        shared_memory_store();
        let key = [7u8; device_proto::aead::KEY_LEN];
        keychain::store_push_key(BID, &key).expect("store");
        let (enc, nonce) = seal(
            &key,
            br#"{"title":"t","body":"b","session_id":"s-1","badge":4}"#,
        );
        let preview = decrypt_push_preview(BID, &enc, &nonce)
            .expect("no store failure")
            .expect("decrypts");
        assert_eq!(preview.session_id.as_deref(), Some("s-1"));
        assert_eq!(preview.badge, Some(4));
    }

    /// The rule the iOS NSE spells out: a malformed badge must not cost the
    /// envelope, because the envelope carries the tap target.
    #[test]
    fn a_malformed_badge_costs_only_the_badge() {
        shared_memory_store();
        let key = [9u8; device_proto::aead::KEY_LEN];
        let bid = "device-badbadge";
        keychain::store_push_key(bid, &key).expect("store");
        let (enc, nonce) = seal(
            &key,
            br#"{"title":"t","body":"b","session_id":"s-9","badge":"lots"}"#,
        );
        let preview = decrypt_push_preview(bid, &enc, &nonce)
            .expect("no store failure")
            .expect("decrypts despite the badge");
        assert_eq!(preview.session_id.as_deref(), Some("s-9"));
        assert_eq!(preview.badge, None);
    }

    #[test]
    fn an_unknown_device_is_absence_not_failure() {
        shared_memory_store();
        let (enc, nonce) = seal(
            &[1u8; device_proto::aead::KEY_LEN],
            br#"{"title":"t","body":"b"}"#,
        );
        assert!(
            decrypt_push_preview("device-never-paired", &enc, &nonce)
                .expect("no store failure")
                .is_none()
        );
    }

    #[test]
    fn the_wrong_key_does_not_decrypt() {
        shared_memory_store();
        let bid = "device-wrongkey";
        keychain::store_push_key(bid, &[3u8; device_proto::aead::KEY_LEN]).expect("store");
        let (enc, nonce) = seal(
            &[4u8; device_proto::aead::KEY_LEN],
            br#"{"title":"t","body":"b"}"#,
        );
        assert!(
            decrypt_push_preview(bid, &enc, &nonce)
                .expect("no store failure")
                .is_none()
        );
    }

    #[test]
    fn a_short_nonce_is_refused_before_the_aead() {
        shared_memory_store();
        let bid = "device-shortnonce";
        keychain::store_push_key(bid, &[5u8; device_proto::aead::KEY_LEN]).expect("store");
        let b64 = base64::engine::general_purpose::STANDARD;
        assert!(
            decrypt_push_preview(bid, &b64.encode([0u8; 32]), &b64.encode([0u8; 4]))
                .expect("no store failure")
                .is_none()
        );
    }

    /// The distinction the whole seam exists to keep: a store that cannot answer
    /// is an error, never "this device has no key".
    #[test]
    fn a_store_failure_is_an_error_not_a_missing_key() {
        let store = shared_memory_store();
        let bid = "device-storefail";
        keychain::store_push_key(bid, &[6u8; device_proto::aead::KEY_LEN]).expect("store");
        let (enc, nonce) = seal(
            &[6u8; device_proto::aead::KEY_LEN],
            br#"{"title":"t","body":"b"}"#,
        );
        store.set_failing(true);
        let result = decrypt_push_preview(bid, &enc, &nonce);
        store.set_failing(false);
        assert!(result.is_err(), "a failed read must not read as absence");
    }
}
