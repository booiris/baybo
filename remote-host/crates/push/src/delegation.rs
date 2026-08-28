//! C-side verification of the push-binding delegation chain.
//!
//! C (this crate) cannot link the gateway/app `device-proto` crate (separate
//! workspace), so it re-implements *verification* of the signatures those sign,
//! against the canonical byte layout in `remote-host-protocol`, which both
//! workspaces link. Signing and verification remain separate, but their input
//! framing has one source of truth.
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
use remote_host_protocol::push::{
    NotifySigningInput, PushTarget, delegation_signing_message, notify_signing_message,
    register_signing_message,
};

/// `device_id` text prefix; the remainder is `hex(device_pubkey)` (32 bytes).
pub const DEVICE_ID_PREFIX: &str = "device-";

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

/// The device (`device_pub`) authorized `gateway_pub` to manage its binding.
pub fn verify_delegation(
    device_pub: &VerifyingKey,
    gateway_pub: &VerifyingKey,
    sig: &Signature,
) -> bool {
    device_pub
        .verify(&delegation_signing_message(&gateway_pub.to_bytes()), sig)
        .is_ok()
}

/// A `/register` signature by the (delegated) `gateway_pub`.
pub fn verify_register(
    gateway_pub: &VerifyingKey,
    device_id: &str,
    target: &PushTarget,
    counter: u64,
    sig: &Signature,
) -> bool {
    gateway_pub
        .verify(&register_signing_message(device_id, target, counter), sig)
        .is_ok()
}

/// A `/notify` signature by the (delegated) `gateway_pub`.
pub fn verify_notify(
    gateway_pub: &VerifyingKey,
    input: &NotifySigningInput<'_>,
    sig: &Signature,
) -> bool {
    gateway_pub
        .verify(&notify_signing_message(input), sig)
        .is_ok()
}

/// Signing side of the same byte layout — TEST ONLY (the gateway/app sign in
/// production via `device-proto`). Used by the push crate's own tests to mint
/// valid signed requests, and proves sign↔verify here are self-consistent. The
/// signer/verifier match with `device-proto` is guarded by the pinned vector in
/// [`tests::verify_accepts_pinned_device_proto_vector`].
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
        device_sk.sign(&delegation_signing_message(&gateway_pub.to_bytes()))
    }
    pub fn sign_register(
        gateway_sk: &SigningKey,
        device_id: &str,
        target: &PushTarget,
        counter: u64,
    ) -> Signature {
        gateway_sk.sign(&register_signing_message(device_id, target, counter))
    }
    pub fn sign_notify(gateway_sk: &SigningKey, input: &NotifySigningInput<'_>) -> Signature {
        gateway_sk.sign(&notify_signing_message(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_host_protocol::push::ApnsEnvironment;

    fn apns_target(token: &str) -> PushTarget {
        PushTarget::Apns {
            token: token.to_string(),
            environment: ApnsEnvironment::Sandbox,
        }
    }

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

        let target = apns_target("tok");
        let reg = test_sign::sign_register(&gateway, &device_id, &target, 5);
        assert!(verify_register(&gpub, &device_id, &target, 5, &reg));
        assert!(!verify_register(&gpub, &device_id, &target, 6, &reg));

        let input = NotifySigningInput {
            device_id: &device_id,
            collapse_key: "collapse",
            enc: "e",
            n: "n",
            bid: &device_id,
            counter: 7,
        };
        let ntf = test_sign::sign_notify(&gateway, &input);
        assert!(verify_notify(&gpub, &input, &ntf));
        assert!(!verify_notify(
            &gpub,
            &NotifySigningInput { enc: "e2", ..input },
            &ntf
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
            &apns_target("apns-tok"),
            42,
            &sig(PIN_REGISTER),
        ));
        assert!(verify_notify(
            &gateway_pub,
            &NotifySigningInput {
                device_id: PIN_DEVICE_ID,
                collapse_key: "collapse-key",
                enc: "ZW5j",
                n: "bm9uY2U",
                bid: PIN_DEVICE_ID,
                counter: 42,
            },
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
    const PIN_REGISTER: &str = "1d2912846d9c0c25dbd696319e0cb78194d960a24d0f006bbe02482b49b7fb2855b4b93b784c290f92c0d57f00af459f55a346ec9af0c572777b13ae3afc8a03";
    const PIN_NOTIFY: &str = "7966784813eaf926ea7852a744291626e3980c5430de7e6f73e640e4e247723570e6775b5f51edc0545bfcc9212cceb88398c3fbb5fc6a70d1f08a55f6462f09";
}
