//! Ed25519 delegation that authenticates push `/register` and `/notify` calls to
//! the operator host (**C**).
//!
//! Relay admission is keyed by `remote_api_key`, and the built-in public proxy's
//! `guest` key may be shared by mutually-distrusting tenants. Push requests carry
//! that key as an edge traffic marker, but C's device-token store cannot use it as
//! a binding boundary: once a caller learns a victim `device_id`, an otherwise
//! unauthenticated overwrite would let it redirect or suppress the victim's push
//! binding and spam `/notify`.
//!
//! This module binds every binding mutation and notification to a signature chain
//! C verifies with **no stored secret and no trust-on-first-use**:
//!
//! - The **device** (P) holds an Ed25519 identity key `D`. Its `device_id` *is*
//!   that public key (`device-<hex(D_pub)>`), so C re-derives `device_id` from
//!   the key carried in the request and the binding self-certifies.
//! - At pairing P signs a **delegation** authorizing the gateway's Ed25519 push
//!   key `G` to manage P's binding.
//! - The **gateway** (A) signs each `/register` and `/notify` with `G`, including
//!   a strictly-increasing `counter` for replay rejection.
//!
//! C checks: `device_id == device-<hex(D_pub)>`, the delegation under `D_pub`,
//! and the request signature under `G_pub`. Only the holder of `D` can authorize a
//! `G`, and only the holder of `G` can mutate/notify the binding — independent of
//! relay admission. C lives in a separate Cargo workspace, but both sides use
//! the canonical signed bytes from `remote-host-protocol`; signing and
//! verification remain separate here and at C.

use zeroize::Zeroize;

pub use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use ed25519_dalek::{Signer, Verifier};
use remote_host_protocol::push::{
    NotifySigningInput, PushTarget, delegation_signing_message, notify_signing_message,
    register_signing_message,
};

use crate::error::ProtoError;

/// `device_id` text prefix. The remainder is `hex(D_pub)` (32 Ed25519 public
/// bytes → 64 lowercase hex chars).
pub const DEVICE_ID_PREFIX: &str = "device-";
/// Ed25519 public key length.
pub const PUBLIC_LEN: usize = 32;
/// Ed25519 signature length.
pub const SIGNATURE_LEN: usize = 64;
/// Ed25519 secret seed length.
pub const SEED_LEN: usize = 32;

/// Mint a fresh Ed25519 identity from the OS CSPRNG. Used for both the device
/// identity (P) and the gateway push-signing key (A).
pub fn generate_signing_key() -> SigningKey {
    use rand::RngExt;
    let mut rng = rand::rng();
    let mut seed: [u8; SEED_LEN] = std::array::from_fn(|_| rng.random_range(0..=255) as u8);
    let key = SigningKey::from_bytes(&seed);
    seed.zeroize();
    key
}

/// The `device_id` for a device verifying key: `device-<hex(pub)>`. This is the
/// only place the mapping is defined; C re-derives the same string and compares.
pub fn device_id_for(device_pub: &VerifyingKey) -> String {
    format!("{DEVICE_ID_PREFIX}{}", hex::encode(device_pub.to_bytes()))
}

/// Recover the device verifying key from a `device_id`, validating the prefix,
/// hex, length, and that the bytes are a valid Ed25519 point.
pub fn device_pubkey_from_id(device_id: &str) -> Result<VerifyingKey, ProtoError> {
    let hex_part = device_id
        .strip_prefix(DEVICE_ID_PREFIX)
        .ok_or(ProtoError::Signature { stage: "id_prefix" })?;
    let bytes = hex::decode(hex_part).map_err(|_| ProtoError::Signature { stage: "id_hex" })?;
    let arr: [u8; PUBLIC_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| ProtoError::Signature { stage: "id_len" })?;
    VerifyingKey::from_bytes(&arr).map_err(|_| ProtoError::Signature { stage: "id_key" })
}

/// Parse a 32-byte Ed25519 public key (e.g. the gateway push key carried on the
/// wire).
pub fn verifying_key_from_bytes(bytes: &[u8]) -> Result<VerifyingKey, ProtoError> {
    let arr: [u8; PUBLIC_LEN] = bytes
        .try_into()
        .map_err(|_| ProtoError::Signature { stage: "pub_len" })?;
    VerifyingKey::from_bytes(&arr).map_err(|_| ProtoError::Signature { stage: "pub_key" })
}

/// Parse a 64-byte Ed25519 signature.
pub fn signature_from_bytes(bytes: &[u8]) -> Result<Signature, ProtoError> {
    let arr: [u8; SIGNATURE_LEN] = bytes
        .try_into()
        .map_err(|_| ProtoError::Signature { stage: "sig_len" })?;
    Ok(Signature::from_bytes(&arr))
}

/// P signs a delegation authorizing `gateway_pub` to manage P's push binding.
/// `device_sk` is the device identity key whose public half is `device_id`.
pub fn sign_delegation(device_sk: &SigningKey, gateway_pub: &VerifyingKey) -> Signature {
    device_sk.sign(&delegation_signing_message(&gateway_pub.to_bytes()))
}

/// Verify a delegation: `device_pub` (i.e. the `device_id` owner) authorized
/// `gateway_pub`.
pub fn verify_delegation(
    device_pub: &VerifyingKey,
    gateway_pub: &VerifyingKey,
    sig: &Signature,
) -> bool {
    device_pub
        .verify(&delegation_signing_message(&gateway_pub.to_bytes()), sig)
        .is_ok()
}

/// A signs a `/register` for `device_id` with its delegated gateway key.
pub fn sign_register(
    gateway_sk: &SigningKey,
    device_id: &str,
    target: &PushTarget,
    counter: u64,
) -> Signature {
    gateway_sk.sign(&register_signing_message(device_id, target, counter))
}

/// Verify a `/register` signature under the (delegated) `gateway_pub`.
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

/// A signs a `/notify` for `device_id` with its delegated gateway key.
pub fn sign_notify(gateway_sk: &SigningKey, input: &NotifySigningInput<'_>) -> Signature {
    gateway_sk.sign(&notify_signing_message(input))
}

/// Verify a `/notify` signature under the (delegated) `gateway_pub`.
pub fn verify_notify(
    gateway_pub: &VerifyingKey,
    input: &NotifySigningInput<'_>,
    sig: &Signature,
) -> bool {
    gateway_pub
        .verify(&notify_signing_message(input), sig)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_host_protocol::push::ApnsEnvironment;

    fn apns_target(token: &str, environment: ApnsEnvironment) -> PushTarget {
        PushTarget::Apns {
            token: token.to_string(),
            environment,
        }
    }

    #[test]
    fn device_id_round_trips_through_key() {
        let d = generate_signing_key();
        let id = device_id_for(&d.verifying_key());
        assert!(id.starts_with("device-"));
        assert_eq!(id.len(), DEVICE_ID_PREFIX.len() + PUBLIC_LEN * 2);
        let recovered = device_pubkey_from_id(&id).unwrap();
        assert_eq!(recovered, d.verifying_key());
    }

    #[test]
    fn malformed_device_ids_are_rejected() {
        assert!(device_pubkey_from_id("nope-deadbeef").is_err()); // wrong prefix
        assert!(device_pubkey_from_id("device-zz").is_err()); // not hex
        assert!(device_pubkey_from_id("device-00").is_err()); // wrong length
    }

    #[test]
    fn delegation_verifies_only_for_the_authorized_gateway() {
        let device = generate_signing_key();
        let gateway = generate_signing_key();
        let other_gateway = generate_signing_key();

        let sig = sign_delegation(&device, &gateway.verifying_key());
        assert!(verify_delegation(
            &device.verifying_key(),
            &gateway.verifying_key(),
            &sig
        ));
        // A different gateway key the device never authorized.
        assert!(!verify_delegation(
            &device.verifying_key(),
            &other_gateway.verifying_key(),
            &sig
        ));
        // A different device did not sign this delegation.
        let impostor = generate_signing_key();
        assert!(!verify_delegation(
            &impostor.verifying_key(),
            &gateway.verifying_key(),
            &sig
        ));
    }

    #[test]
    fn register_signature_binds_every_field() {
        let gateway = generate_signing_key();
        let gp = gateway.verifying_key();
        let target = apns_target("tok", ApnsEnvironment::Sandbox);
        let sig = sign_register(&gateway, "device-aa", &target, 7);
        assert!(verify_register(&gp, "device-aa", &target, 7, &sig));
        // Each field is covered: flipping any one breaks verification.
        assert!(!verify_register(&gp, "device-bb", &target, 7, &sig));
        assert!(!verify_register(
            &gp,
            "device-aa",
            &apns_target("tok2", ApnsEnvironment::Sandbox),
            7,
            &sig
        ));
        assert!(!verify_register(
            &gp,
            "device-aa",
            &apns_target("tok", ApnsEnvironment::Production),
            7,
            &sig
        ));
        assert!(!verify_register(
            &gp,
            "device-aa",
            &PushTarget::Fcm {
                token: "tok".into(),
            },
            7,
            &sig
        ));
        assert!(!verify_register(&gp, "device-aa", &target, 8, &sig));
    }

    #[test]
    fn notify_signature_binds_every_field() {
        let gateway = generate_signing_key();
        let gp = gateway.verifying_key();
        let input = NotifySigningInput {
            device_id: "device-aa",
            collapse_key: "collapse",
            enc: "enc",
            n: "n",
            bid: "device-aa",
            counter: 42,
        };
        let sig = sign_notify(&gateway, &input);
        assert!(verify_notify(&gp, &input, &sig));
        assert!(!verify_notify(
            &gp,
            &NotifySigningInput {
                collapse_key: "collapse2",
                ..input
            },
            &sig
        ));
        assert!(!verify_notify(
            &gp,
            &NotifySigningInput {
                enc: "enc2",
                ..input
            },
            &sig
        ));
        assert!(!verify_notify(
            &gp,
            &NotifySigningInput { n: "n2", ..input },
            &sig
        ));
        assert!(!verify_notify(
            &gp,
            &NotifySigningInput {
                bid: "device-bb",
                ..input
            },
            &sig
        ));
        assert!(!verify_notify(
            &gp,
            &NotifySigningInput {
                counter: 43,
                ..input
            },
            &sig
        ));
    }

    #[test]
    fn contexts_are_not_cross_verifiable() {
        // A register signature must not verify as a notify (and vice-versa), even
        // with otherwise-matching fields — the domain-separation contexts differ.
        let gateway = generate_signing_key();
        let gp = gateway.verifying_key();
        let reg = sign_register(
            &gateway,
            "device-aa",
            &apns_target("x", ApnsEnvironment::Sandbox),
            1,
        );
        assert!(!verify_notify(
            &gp,
            &NotifySigningInput {
                device_id: "device-aa",
                collapse_key: "x",
                enc: "x",
                n: "x",
                bid: "device-aa",
                counter: 1,
            },
            &reg
        ));
    }

    /// Pin the wire byte layout to fixed vectors. The remote-host push crate
    /// verifies the same vectors, so any drift breaks a test before it silently
    /// breaks every push.
    #[test]
    fn pinned_vector() {
        let device = SigningKey::from_bytes(&[1u8; 32]);
        let gateway = SigningKey::from_bytes(&[2u8; 32]);
        let device_id = device_id_for(&device.verifying_key());
        assert_eq!(
            device_id,
            "device-8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c"
        );
        assert_eq!(
            hex::encode(gateway.verifying_key().to_bytes()),
            "8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394"
        );
        assert_eq!(
            hex::encode(sign_delegation(&device, &gateway.verifying_key()).to_bytes()),
            "3d3d3bc2446e76a660d2ea8cfe0ce39d3ffaec4586afe941b1c8b5b09183143b1aea130de49118eb8da5f30c4069f884e0b53902ac7faf4654a0a9a600df170c"
        );
        assert_eq!(
            hex::encode(
                sign_register(
                    &gateway,
                    &device_id,
                    &apns_target("apns-tok", ApnsEnvironment::Sandbox),
                    42,
                )
                .to_bytes()
            ),
            "1d2912846d9c0c25dbd696319e0cb78194d960a24d0f006bbe02482b49b7fb2855b4b93b784c290f92c0d57f00af459f55a346ec9af0c572777b13ae3afc8a03"
        );
        assert_eq!(
            hex::encode(
                sign_notify(
                    &gateway,
                    &NotifySigningInput {
                        device_id: &device_id,
                        collapse_key: "collapse-key",
                        enc: "ZW5j",
                        n: "bm9uY2U",
                        bid: &device_id,
                        counter: 42,
                    },
                )
                .to_bytes()
            ),
            "7966784813eaf926ea7852a744291626e3980c5430de7e6f73e640e4e247723570e6775b5f51edc0545bfcc9212cceb88398c3fbb5fc6a70d1f08a55f6462f09"
        );
    }

    #[test]
    fn full_chain_device_authorizes_gateway_to_notify() {
        // End-to-end: device delegates to a gateway, gateway signs a notify, and a
        // verifier with only the wire material (device_id, gateway_pub, both sigs)
        // accepts — modeling exactly what C does.
        let device = generate_signing_key();
        let gateway = generate_signing_key();
        let device_id = device_id_for(&device.verifying_key());

        let deleg = sign_delegation(&device, &gateway.verifying_key());
        let notify_input = NotifySigningInput {
            device_id: &device_id,
            collapse_key: "collapse",
            enc: "enc",
            n: "n",
            bid: &device_id,
            counter: 1,
        };
        let notify = sign_notify(&gateway, &notify_input);

        // C-side: recover device key from device_id, check the chain.
        let dpub = device_pubkey_from_id(&device_id).unwrap();
        assert!(verify_delegation(&dpub, &gateway.verifying_key(), &deleg));
        assert!(verify_notify(
            &gateway.verifying_key(),
            &notify_input,
            &notify
        ));
    }
}
