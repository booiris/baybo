//! Wire messages exchanged inside the SPAKE2 K-channel at pairing.
//!
//! Once both ends derive the master secret and the channel key
//! ([`crate::kdf`]), they swap exactly two AEAD-protected messages over the
//! rendezvous:
//!
//! 1. [`DeviceHello`] (P → A): the app's long-term Noise static public key and
//!    its push registration (`apns_token`, `apns_env`, the per-binding
//!    `device_id`).
//! 2. [`GatewayWelcome`] (A → P): the gateway's static public key plus the
//!    routing the app needs to reach it later — the C-assigned
//!    `relay_node_id`, direct-reachability candidates, the owning `user_id`,
//!    and the retained pairing code (the operator approval handle).
//!
//! These travel Rust↔Rust only (both ends link this crate), so MessagePack is
//! the codec; only the [`crate::aead`] preview framing needs cross-language
//! pinning. The channel key authenticates them — C relays opaque ciphertext
//! and cannot read or forge a static key.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::aead::{self, KEY_LEN};
use crate::error::ProtoError;

/// Which APNs environment the app's device token is bound to. A sandbox token
/// is rejected by the production host and vice versa, so it is tracked
/// per-device from pairing onward (never guessed at send time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApnsEnv {
    Sandbox,
    Production,
}

/// App → gateway, inside the K-channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHello {
    /// Client-generated, per-binding (one row per `(user_id, device_id)`).
    pub device_id: String,
    /// Human label for the device list ("Booiris iPhone").
    pub label: String,
    /// The app's long-term Noise static public key.
    pub static_pubkey: [u8; KEY_LEN],
    /// APNs device token (gateway-mediated registration relays it to C).
    pub apns_token: String,
    /// Environment the token is bound to.
    pub apns_env: ApnsEnv,
}

/// Gateway → app, inside the K-channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayWelcome {
    /// The gateway's long-term Noise static public key.
    pub static_pubkey: [u8; KEY_LEN],
    /// C-assigned, non-secret routing id the app uses to reach a NAT'd A via
    /// the blind relay. P never holds A's `instance_key`.
    pub relay_node_id: String,
    /// Direct-reachability candidates (LAN / Tailscale / reverse proxy) the
    /// app tries before falling back to the relay.
    pub direct_candidates: Vec<String>,
    /// Owning principal on the gateway.
    pub user_id: String,
    /// The SPAKE2 code this device paired under, retained for audit / the
    /// operator's device list (no longer an approval handle).
    pub pairing_code: String,
    /// The device's bearer token for the scoped REST/WS surface. Active the
    /// moment this `GatewayWelcome` is sent — the gateway only seals it after
    /// both the phone user and the operator confirmed the pairing.
    pub auth_token: String,
}

/// App → gateway, inside the K-channel: the phone user's pairing decision after
/// comparing the displayed confirmation code (see [`crate::kdf::derive_confirm_code`]).
/// Sealed under the channel key so a blind relay can neither forge an acceptance
/// nor flip a decline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConfirm {
    pub accepted: bool,
}

/// On-wire envelope for the device-pairing handshake over the `/v1/device/pair`
/// WebSocket (each variant is one binary frame, MessagePack-encoded):
///
/// 1. P → A [`PairFrame::Hello`] — the pairing code + the app's SPAKE2 message.
/// 2. A → P [`PairFrame::PakeReply`] — A's SPAKE2 message (or
///    [`PairFrame::Reject`] on a bad/expired code).
/// 3. P → A [`PairFrame::Sealed`] — the [`DeviceHello`], AEAD-sealed under the
///    derived channel key.
/// 4. P → A [`PairFrame::Sealed`] — the [`DeviceConfirm`] (the phone user's
///    decision after comparing the confirmation code on both screens).
/// 5. A → P [`PairFrame::Sealed`] — the [`GatewayWelcome`], sealed and sent
///    only once the operator has confirmed too (or [`PairFrame::Reject`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PairFrame {
    Hello { code: String, pake: Vec<u8> },
    PakeReply { pake: Vec<u8> },
    Sealed { nonce: Vec<u8>, ciphertext: Vec<u8> },
    Reject { reason: String },
}

/// Encode a pairing message to MessagePack (named fields).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    rmp_serde::to_vec_named(msg).map_err(|e| ProtoError::Codec(e.to_string()))
}

/// Decode a pairing message from MessagePack.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    rmp_serde::from_slice(bytes).map_err(|e| ProtoError::Codec(e.to_string()))
}

/// Encode `msg` and AEAD-seal it under the K-channel key. Returns
/// `(nonce, ciphertext)` to relay through C.
pub fn seal_msg<T: Serialize>(
    channel_key: &[u8; KEY_LEN],
    msg: &T,
) -> Result<(Vec<u8>, Vec<u8>), ProtoError> {
    aead::seal(channel_key, &encode(msg)?)
}

/// Open a sealed pairing message and decode it.
pub fn open_msg<T: DeserializeOwned>(
    channel_key: &[u8; KEY_LEN],
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<T, ProtoError> {
    decode(&aead::open(channel_key, nonce, ciphertext)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdf::derive_pair_keys;
    use crate::pake::Pake;

    #[test]
    fn device_confirm_round_trips() {
        for accepted in [true, false] {
            let c = DeviceConfirm { accepted };
            let decoded: DeviceConfirm = decode(&encode(&c).unwrap()).unwrap();
            assert_eq!(c, decoded);
        }
    }

    #[test]
    fn device_hello_round_trips() {
        let hello = DeviceHello {
            device_id: "dev-123".into(),
            label: "Booiris iPhone".into(),
            static_pubkey: [5u8; KEY_LEN],
            apns_token: "abc123".into(),
            apns_env: ApnsEnv::Sandbox,
        };
        let decoded: DeviceHello = decode(&encode(&hello).unwrap()).unwrap();
        assert_eq!(hello, decoded);
    }

    #[test]
    fn gateway_welcome_round_trips() {
        let welcome = GatewayWelcome {
            static_pubkey: [9u8; KEY_LEN],
            relay_node_id: "node-xyz".into(),
            direct_candidates: vec!["wss://baybo.lan:8889".into()],
            user_id: "user-1".into(),
            pairing_code: "WORMHOLE-7-foo-bar".into(),
            auth_token: "deadbeef".into(),
        };
        let decoded: GatewayWelcome = decode(&encode(&welcome).unwrap()).unwrap();
        assert_eq!(welcome, decoded);
    }

    #[test]
    fn pair_frames_round_trip() {
        for frame in [
            PairFrame::Hello {
                code: "ABC123".into(),
                pake: vec![1, 2, 3],
            },
            PairFrame::PakeReply { pake: vec![4, 5] },
            PairFrame::Sealed {
                nonce: vec![0; 12],
                ciphertext: vec![9; 40],
            },
            PairFrame::Reject {
                reason: "expired code".into(),
            },
        ] {
            let decoded: PairFrame = decode(&encode(&frame).unwrap()).unwrap();
            assert_eq!(frame, decoded);
        }
    }

    /// End-to-end: SPAKE2 → channel key → sealed pairing exchange both ways,
    /// exactly the phase-1 pairing wire path (minus the rendezvous relay).
    #[test]
    fn sealed_exchange_over_pairing_channel() {
        let (app, app_msg) = Pake::start_app("code-shared");
        let (gw, gw_msg) = Pake::start_gateway("code-shared");
        let app_k = app.finish(&gw_msg).unwrap();
        let gw_k = gw.finish(&app_msg).unwrap();
        let app_keys = derive_pair_keys(&app_k).unwrap();
        let gw_keys = derive_pair_keys(&gw_k).unwrap();
        assert_eq!(app_keys.channel_key, gw_keys.channel_key);

        // P → A
        let hello = DeviceHello {
            device_id: "dev-1".into(),
            label: "iPhone".into(),
            static_pubkey: [1u8; KEY_LEN],
            apns_token: "tok".into(),
            apns_env: ApnsEnv::Production,
        };
        let (n, ct) = seal_msg(&app_keys.channel_key, &hello).unwrap();
        let got: DeviceHello = open_msg(&gw_keys.channel_key, &n, &ct).unwrap();
        assert_eq!(got, hello);

        // A → P
        let welcome = GatewayWelcome {
            static_pubkey: [2u8; KEY_LEN],
            relay_node_id: "n1".into(),
            direct_candidates: vec![],
            user_id: "u1".into(),
            pairing_code: "code-shared".into(),
            auth_token: "tok".into(),
        };
        let (n, ct) = seal_msg(&gw_keys.channel_key, &welcome).unwrap();
        let got: GatewayWelcome = open_msg(&app_keys.channel_key, &n, &ct).unwrap();
        assert_eq!(got, welcome);
    }

    #[test]
    fn wrong_channel_key_cannot_open() {
        let keys = derive_pair_keys(b"master").unwrap();
        let hello = DeviceHello {
            device_id: "d".into(),
            label: "l".into(),
            static_pubkey: [0u8; KEY_LEN],
            apns_token: "t".into(),
            apns_env: ApnsEnv::Sandbox,
        };
        let (n, ct) = seal_msg(&keys.channel_key, &hello).unwrap();
        let other = derive_pair_keys(b"different-master").unwrap();
        assert!(open_msg::<DeviceHello>(&other.channel_key, &n, &ct).is_err());
    }
}
