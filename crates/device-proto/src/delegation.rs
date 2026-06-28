//! Ed25519 delegation that authenticates push `/register` and `/notify` to the
//! operator host (**C**) under a *shared* `remote_api_key` — most importantly the
//! built-in `guest` trial key, which many mutually-distrusting tenants use.
//!
//! C's device-token store is keyed by `(remote_api_key, device_id)` and its
//! `/register` is an unconditional overwrite gated only by admission. Under a
//! shared key that is **no isolation at all**: once a tenant learns a victim
//! `device_id` it can overwrite, redirect, or suppress the victim's APNs binding,
//! and spam `/notify`. Key-partitioning cannot fix this because C cannot
//! authenticate *which* tenant a request came from beyond the shared key.
//!
//! This module binds every binding mutation and notification to a signature chain
//! C verifies with **no stored secret and no trust-on-first-use**:
//!
//! - The **device** (P) holds an Ed25519 identity key `D`. Its `device_id` *is*
//!   that public key (`ios-<hex(D_pub)>`), so C re-derives `device_id` from the
//!   key carried in the request and the binding self-certifies.
//! - At pairing P signs a **delegation** authorizing the gateway's Ed25519 push
//!   key `G` to manage P's binding.
//! - The **gateway** (A) signs each `/register` and `/notify` with `G`, including
//!   a strictly-increasing `counter` for replay rejection.
//!
//! C checks: `device_id == ios-<hex(D_pub)>`, the delegation under `D_pub`, and
//! the request signature under `G_pub`. Only the holder of `D` can authorize a
//! `G`, and only the holder of `G` can mutate/notify the binding — independent of
//! the shared admission key. C (a separate Cargo workspace that cannot link this
//! crate) re-implements verification against the byte layout pinned here; the
//! context strings, the `env` byte mapping, and the length-prefix framing below
//! are the cross-implementation contract.

use zeroize::Zeroize;

pub use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use ed25519_dalek::{Signer, Verifier};

use crate::error::ProtoError;

/// `device_id` text prefix. The remainder is `hex(D_pub)` (32 Ed25519 public
/// bytes → 64 lowercase hex chars).
pub const DEVICE_ID_PREFIX: &str = "ios-";
/// Ed25519 public key length.
pub const PUBLIC_LEN: usize = 32;
/// Ed25519 signature length.
pub const SIGNATURE_LEN: usize = 64;
/// Ed25519 secret seed length.
pub const SEED_LEN: usize = 32;

/// Domain-separation contexts. Distinct per message kind so a signature minted
/// for one role can never verify as another.
const DELEGATION_CONTEXT: &[u8] = b"baybo/push/delegation/v1";
const REGISTER_CONTEXT: &[u8] = b"baybo/push/register/v1";
const NOTIFY_CONTEXT: &[u8] = b"baybo/push/notify/v1";

/// Canonical `env` byte in the signed register message: APNs sandbox vs
/// production. Pinned here so C's independent verifier maps identically.
pub const ENV_SANDBOX: u8 = 0;
pub const ENV_PRODUCTION: u8 = 1;

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

/// The `device_id` for a device verifying key: `ios-<hex(pub)>`. This is the
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

/// Append `bytes` as a `u32`-LE length prefix followed by the bytes, so the
/// signed message is an unambiguous concatenation of variable-length fields.
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

/// P signs a delegation authorizing `gateway_pub` to manage P's push binding.
/// `device_sk` is the device identity key whose public half is `device_id`.
pub fn sign_delegation(device_sk: &SigningKey, gateway_pub: &VerifyingKey) -> Signature {
    device_sk.sign(&delegation_message(gateway_pub))
}

/// Verify a delegation: `device_pub` (i.e. the `device_id` owner) authorized
/// `gateway_pub`.
pub fn verify_delegation(
    device_pub: &VerifyingKey,
    gateway_pub: &VerifyingKey,
    sig: &Signature,
) -> bool {
    device_pub
        .verify(&delegation_message(gateway_pub), sig)
        .is_ok()
}

/// A signs a `/register` for `device_id` with its delegated gateway key.
pub fn sign_register(
    gateway_sk: &SigningKey,
    device_id: &str,
    apns_token: &str,
    env: u8,
    counter: u64,
) -> Signature {
    gateway_sk.sign(&register_message(device_id, apns_token, env, counter))
}

/// Verify a `/register` signature under the (delegated) `gateway_pub`.
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

/// A signs a `/notify` for `device_id` with its delegated gateway key.
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

/// Verify a `/notify` signature under the (delegated) `gateway_pub`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_round_trips_through_key() {
        let d = generate_signing_key();
        let id = device_id_for(&d.verifying_key());
        assert!(id.starts_with("ios-"));
        assert_eq!(id.len(), DEVICE_ID_PREFIX.len() + PUBLIC_LEN * 2);
        let recovered = device_pubkey_from_id(&id).unwrap();
        assert_eq!(recovered, d.verifying_key());
    }

    #[test]
    fn malformed_device_ids_are_rejected() {
        assert!(device_pubkey_from_id("nope-deadbeef").is_err()); // wrong prefix
        assert!(device_pubkey_from_id("ios-zz").is_err()); // not hex
        assert!(device_pubkey_from_id("ios-00").is_err()); // wrong length
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
        let sig = sign_register(&gateway, "ios-aa", "tok", ENV_SANDBOX, 7);
        assert!(verify_register(&gp, "ios-aa", "tok", ENV_SANDBOX, 7, &sig));
        // Each field is covered: flipping any one breaks verification.
        assert!(!verify_register(&gp, "ios-bb", "tok", ENV_SANDBOX, 7, &sig));
        assert!(!verify_register(
            &gp,
            "ios-aa",
            "tok2",
            ENV_SANDBOX,
            7,
            &sig
        ));
        assert!(!verify_register(
            &gp,
            "ios-aa",
            "tok",
            ENV_PRODUCTION,
            7,
            &sig
        ));
        assert!(!verify_register(&gp, "ios-aa", "tok", ENV_SANDBOX, 8, &sig));
    }

    #[test]
    fn notify_signature_binds_every_field() {
        let gateway = generate_signing_key();
        let gp = gateway.verifying_key();
        let sig = sign_notify(&gateway, "ios-aa", "enc", "n", "ios-aa", 42);
        assert!(verify_notify(&gp, "ios-aa", "enc", "n", "ios-aa", 42, &sig));
        assert!(!verify_notify(
            &gp, "ios-aa", "enc2", "n", "ios-aa", 42, &sig
        ));
        assert!(!verify_notify(
            &gp, "ios-aa", "enc", "n2", "ios-aa", 42, &sig
        ));
        assert!(!verify_notify(
            &gp, "ios-aa", "enc", "n", "ios-bb", 42, &sig
        ));
        assert!(!verify_notify(
            &gp, "ios-aa", "enc", "n", "ios-aa", 43, &sig
        ));
    }

    #[test]
    fn contexts_are_not_cross_verifiable() {
        // A register signature must not verify as a notify (and vice-versa), even
        // with otherwise-matching fields — the domain-separation contexts differ.
        let gateway = generate_signing_key();
        let gp = gateway.verifying_key();
        let reg = sign_register(&gateway, "ios-aa", "x", ENV_SANDBOX, 1);
        assert!(!verify_notify(&gp, "ios-aa", "x", "x", "ios-aa", 1, &reg));
    }

    /// Pin the wire byte layout to fixed vectors. The remote-host push crate
    /// (a separate workspace that re-implements verification) asserts the SAME
    /// vectors in `verify_accepts_pinned_device_proto_vector`, so any drift on
    /// either side breaks a test before it silently breaks every push.
    #[test]
    fn pinned_vector() {
        let device = SigningKey::from_bytes(&[1u8; 32]);
        let gateway = SigningKey::from_bytes(&[2u8; 32]);
        let device_id = device_id_for(&device.verifying_key());
        assert_eq!(
            device_id,
            "ios-8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c"
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
                sign_register(&gateway, &device_id, "apns-tok", ENV_SANDBOX, 42).to_bytes()
            ),
            "29f370f97837868aafc24f4d99fc02971a342416c534d5e569115bfecb42df48933b5a6afe3d88b8d3391c72ce69fb3c32324dc170b6bf587e1b65b0aa4d320a"
        );
        assert_eq!(
            hex::encode(
                sign_notify(&gateway, &device_id, "ZW5j", "bm9uY2U", &device_id, 42).to_bytes()
            ),
            "8f96a51b0b1d2fcceff1f851039229aa888e420ec6614ff227419ea9a287ecda9817261179bc35a0efebe3337875a731a2c731f9035fdc8fd64eb988d250ee01"
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
        let notify = sign_notify(&gateway, &device_id, "enc", "n", &device_id, 1);

        // C-side: recover device key from device_id, check the chain.
        let dpub = device_pubkey_from_id(&device_id).unwrap();
        assert!(verify_delegation(&dpub, &gateway.verifying_key(), &deleg));
        assert!(verify_notify(
            &gateway.verifying_key(),
            &device_id,
            "enc",
            "n",
            &device_id,
            1,
            &notify
        ));
    }
}
