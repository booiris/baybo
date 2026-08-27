//! C-side verification of the push-binding delegation chain.
//!
//! C (this crate) cannot link the gateway/app `device-proto` crate (separate
//! workspace), so it re-implements *verification* of the signatures those sign,
//! against `device-proto`'s byte layout. The domain-separation context strings
//! come from the shared `remote-host-protocol` crate both workspaces link (a
//! single source of truth); the `env` byte mapping and the length-prefix framing
//! below are re-implemented here and MUST match `device-proto` exactly, or every
//! signature will (safely) fail to verify — drift in that part is caught by the
//! pinned cross-impl vector in `verify_accepts_pinned_device_proto_vector`.
//!
//! C verifies, with no stored secret and no trust-on-first-use:
//! - `device_id == device-<hex(device_pubkey)>` — the binding self-certifies;
//! - the **delegation** under the device key (device authorized a gateway key);
//! - each `/register` and `/notify` **request signature** under that gateway key.
//!
//! The remote API key carried by push requests is an edge traffic marker, not a
//! device-binding boundary — this chain is the binding authorization. No one can
//! overwrite, redirect, suppress, or spam another's binding even knowing its
//! `device_id`: they cannot forge the device's delegation or the gateway's
//! request signature.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use remote_host_protocol::push::{DELEGATION_CONTEXT, NOTIFY_CONTEXT, REGISTER_CONTEXT};

/// `device_id` text prefix; the remainder is `hex(device_pubkey)` (32 bytes).
pub const DEVICE_ID_PREFIX: &str = "device-";

/// Canonical `env` byte in the signed register message (matches `device-proto`).
pub const ENV_SANDBOX: u8 = 0;
pub const ENV_PRODUCTION: u8 = 1;

const PUBLIC_LEN: usize = 32;
const SIGNATURE_LEN: usize = 64;

/// Recover the device verifying key from a `device_id` (`device-<hex(pub)>`).
pub fn device_pubkey_from_id(device_id: &str) -> Option<VerifyingKey> {
    let hex_part = device_id.strip_prefix(DEVICE_ID_PREFIX)?;
    let bytes = hex::decode(hex_part).ok()?;
    let arr: [u8; PUBLIC_LEN] = bytes.as_slice().try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

/// Parse a 32-byte Ed25519 public key.
pub fn verifying_key_from_bytes(bytes: &[u8]) -> Option<VerifyingKey> {
    let arr: [u8; PUBLIC_LEN] = bytes.try_into().ok()?;
    VerifyingKey::from_bytes(&arr).ok()
}

/// Parse a 64-byte Ed25519 signature.
pub fn signature_from_bytes(bytes: &[u8]) -> Option<Signature> {
    let arr: [u8; SIGNATURE_LEN] = bytes.try_into().ok()?;
    Some(Signature::from_bytes(&arr))
}

fn push_field(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

fn delegation_message(gateway_pub: &VerifyingKey) -> Vec<u8> {
    let mut m = Vec::with_capacity(DELEGATION_CONTEXT.len() + PUBLIC_LEN);
    m.extend_from_slice(DELEGATION_CONTEXT);
    m.extend_from_slice(&gateway_pub.to_bytes());
    m
}

fn register_message(device_id: &str, apns_token: &str, env: u8, counter: u64) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(REGISTER_CONTEXT);
    push_field(&mut m, device_id.as_bytes());
    push_field(&mut m, apns_token.as_bytes());
    m.push(env);
    m.extend_from_slice(&counter.to_le_bytes());
    m
}

fn notify_message(device_id: &str, enc: &str, n: &str, bid: &str, counter: u64) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(NOTIFY_CONTEXT);
    push_field(&mut m, device_id.as_bytes());
    push_field(&mut m, enc.as_bytes());
    push_field(&mut m, n.as_bytes());
    push_field(&mut m, bid.as_bytes());
    m.extend_from_slice(&counter.to_le_bytes());
    m
}

/// The device (`device_pub`) authorized `gateway_pub` to manage its binding.
pub fn verify_delegation(
    device_pub: &VerifyingKey,
    gateway_pub: &VerifyingKey,
    sig: &Signature,
) -> bool {
    device_pub
        .verify(&delegation_message(gateway_pub), sig)
        .is_ok()
}

/// A `/register` signature by the (delegated) `gateway_pub`.
pub fn verify_register(
    gateway_pub: &VerifyingKey,
    device_id: &str,
    apns_token: &str,
    env: u8,
    counter: u64,
    sig: &Signature,
) -> bool {
    gateway_pub
        .verify(&register_message(device_id, apns_token, env, counter), sig)
        .is_ok()
}

/// A `/notify` signature by the (delegated) `gateway_pub`.
pub fn verify_notify(
    gateway_pub: &VerifyingKey,
    device_id: &str,
    enc: &str,
    n: &str,
    bid: &str,
    counter: u64,
    sig: &Signature,
) -> bool {
    gateway_pub
        .verify(&notify_message(device_id, enc, n, bid, counter), sig)
        .is_ok()
}

/// Signing side of the same byte layout — TEST ONLY (the gateway/app sign in
/// production via `device-proto`). Used by the push crate's own tests to mint
/// valid signed requests, and proves sign↔verify here are self-consistent. The
/// cross-implementation match with `device-proto` is guarded by the pinned
/// vector in [`tests::verify_accepts_pinned_device_proto_vector`].
#[cfg(test)]
pub(crate) mod test_sign {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }
    pub fn device_id_for(vk: &VerifyingKey) -> String {
        format!("{DEVICE_ID_PREFIX}{}", hex::encode(vk.to_bytes()))
    }
    pub fn sign_delegation(device_sk: &SigningKey, gateway_pub: &VerifyingKey) -> Signature {
        device_sk.sign(&delegation_message(gateway_pub))
    }
    pub fn sign_register(
        gateway_sk: &SigningKey,
        device_id: &str,
        apns_token: &str,
        env: u8,
        counter: u64,
    ) -> Signature {
        gateway_sk.sign(&register_message(device_id, apns_token, env, counter))
    }
    pub fn sign_notify(
        gateway_sk: &SigningKey,
        device_id: &str,
        enc: &str,
        n: &str,
        bid: &str,
        counter: u64,
    ) -> Signature {
        gateway_sk.sign(&notify_message(device_id, enc, n, bid, counter))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_round_trips_and_rejects_tampering() {
        let device = test_sign::signing_key(1);
        let gateway = test_sign::signing_key(2);
        let dpub = device.verifying_key();
        let gpub = gateway.verifying_key();
        let device_id = test_sign::device_id_for(&dpub);

        let deleg = test_sign::sign_delegation(&device, &gpub);
        assert!(verify_delegation(&dpub, &gpub, &deleg));
        assert!(!verify_delegation(
            &dpub,
            &test_sign::signing_key(9).verifying_key(),
            &deleg
        ));

        let reg = test_sign::sign_register(&gateway, &device_id, "tok", ENV_SANDBOX, 5);
        assert!(verify_register(
            &gpub,
            &device_id,
            "tok",
            ENV_SANDBOX,
            5,
            &reg
        ));
        assert!(!verify_register(
            &gpub,
            &device_id,
            "tok",
            ENV_SANDBOX,
            6,
            &reg
        ));

        let ntf = test_sign::sign_notify(&gateway, &device_id, "e", "n", &device_id, 7);
        assert!(verify_notify(
            &gpub, &device_id, "e", "n", &device_id, 7, &ntf
        ));
        assert!(!verify_notify(
            &gpub, &device_id, "e2", "n", &device_id, 7, &ntf
        ));
    }

    #[test]
    fn device_id_round_trips_through_pubkey() {
        let device = test_sign::signing_key(3);
        let id = test_sign::device_id_for(&device.verifying_key());
        assert_eq!(device_pubkey_from_id(&id).unwrap(), device.verifying_key());
        assert!(device_pubkey_from_id("nope").is_none());
    }

    /// Cross-implementation guard: signatures produced by `device-proto`'s signer
    /// (the gateway/app side) for fixed seeds + inputs MUST verify here. The hex
    /// is the pinned output of `device-proto`'s `delegation` for device seed
    /// `[1u8;32]`, gateway seed `[2u8;32]`. If either side's byte layout drifts,
    /// this fails (and all real pushes would silently stop verifying).
    #[test]
    fn verify_accepts_pinned_device_proto_vector() {
        let sig = |h: &str| signature_from_bytes(&hex::decode(h).unwrap()).unwrap();
        let device_pub = device_pubkey_from_id(PIN_DEVICE_ID).unwrap();
        let gateway_pub = verifying_key_from_bytes(&hex::decode(PIN_GATEWAY_PUB).unwrap()).unwrap();

        assert!(
            verify_delegation(&device_pub, &gateway_pub, &sig(PIN_DELEGATION)),
            "device-proto delegation must verify (byte-layout contract)",
        );
        assert!(verify_register(
            &gateway_pub,
            PIN_DEVICE_ID,
            "apns-tok",
            ENV_SANDBOX,
            42,
            &sig(PIN_REGISTER),
        ));
        assert!(verify_notify(
            &gateway_pub,
            PIN_DEVICE_ID,
            "ZW5j",
            "bm9uY2U",
            PIN_DEVICE_ID,
            42,
            &sig(PIN_NOTIFY),
        ));
    }

    // Pinned outputs of `device-proto`'s signer for device seed [1u8;32], gateway
    // seed [2u8;32] (see device-proto's `delegation::tests::pinned_vector`). Any
    // byte-layout drift on either side breaks this test before it breaks prod.
    const PIN_DEVICE_ID: &str =
        "device-8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c";
    const PIN_GATEWAY_PUB: &str =
        "8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394";
    const PIN_DELEGATION: &str = "3d3d3bc2446e76a660d2ea8cfe0ce39d3ffaec4586afe941b1c8b5b09183143b1aea130de49118eb8da5f30c4069f884e0b53902ac7faf4654a0a9a600df170c";
    const PIN_REGISTER: &str = "bd2f828205bcdf66a7676b9a073e74b2ad546de3577adff293ac63eb65f81c2623e1d9e7cbfb2bd66038413f73a0731e62f979b719aac5db8aee2f3c66c28b02";
    const PIN_NOTIFY: &str = "0cfd42d4b7af3d5c361993a4c9adb0afcf416931b900f57d3dab37a88c00ea801c2a3a3ca61062178a2958793c50938a1303e1b6ab7c3770df6e7bab31a13a0f";
}
