//! Wire messages exchanged during the XXpsk0 device-pairing handshake.
//!
//! The handshake (see [`crate::psk_pair`]) runs over the relay rendezvous as a
//! sequence of [`PairFrame`]s (each one binary frame, MessagePack-encoded):
//!
//! 1. P → A [`PairFrame::Hello`] — the public `rendezvous_id` + the app's first
//!    Noise handshake message (`msg1`, `e`).
//! 2. A → P [`PairFrame::HandshakeReply`] — the gateway's `msg2` (`e, ee, s,
//!    es`); the app now holds the gateway's static (as an XX token, in `h`).
//! 3. P → A [`PairFrame::HandshakeFinal`] — the app's `msg3` (`s, se`), whose
//!    Noise payload is the [`DeviceHello`] body. Both ends are now in transport
//!    mode and share the final handshake hash `h`.
//! 4. P → A [`PairFrame::Sealed`] — the [`DeviceConfirm`] (the phone user's
//!    decision), a Noise **transport** message (implicit nonce).
//! 5. A → P [`PairFrame::Sealed`] — the [`GatewayWelcome`], a transport
//!    message, sent only once the operator confirmed too (or
//!    [`PairFrame::Reject`]).
//!
//! The statics are exchanged in-band as XX handshake tokens, so neither
//! [`DeviceHello`] nor [`GatewayWelcome`] carries a `static_pubkey` field — each
//! side reads the peer's authenticated static from the handshake itself
//! ([`crate::psk_pair::PskTransport::remote_static`]). The relay sees only the
//! public `rendezvous_id` and opaque Noise frames; lacking the PSK it can
//! neither read nor forge any of this.
//!
//! These travel Rust↔Rust only (both ends link this crate), so MessagePack is
//! the codec.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

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

/// App → gateway, as the Noise payload of the app's `msg3` (so it is
/// authenticated by the app static that rides the same message). The app's
/// static itself is an XX handshake token, not a field here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHello {
    /// Client-generated, per-binding (one row per `(user_id, device_id)`).
    pub device_id: String,
    /// APNs device token (gateway-mediated registration relays it to C).
    pub apns_token: String,
    /// Environment the token is bound to.
    pub apns_env: ApnsEnv,
}

/// Gateway → app, as a Noise transport message. The gateway's static is learned
/// in-band as an XX token, so it is not a field here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayWelcome {
    /// Non-secret routing id the app uses to reach a NAT'd A via the blind
    /// relay (the gateway's stable `relay_node_id`). P never holds A's
    /// `instance_key`. Empty when the gateway has no relay configured.
    pub relay_node_id: String,
    /// Base WS URL of the blind relay (C), e.g. `wss://proxy.baybo.space`. The app
    /// dials `{relay_url}/content/join/{relay_node_id}` to reach the (possibly
    /// NAT'd) gateway for content. Empty when relay is off.
    #[serde(default)]
    pub relay_url: String,
    /// Owning principal on the gateway.
    pub user_id: String,
    /// The **public** rendezvous id this device paired under, retained as an
    /// audit / device-list handle. Not secret (the relay saw it); the QR secret
    /// is never carried here.
    pub rendezvous_id: String,
    /// The device's bearer token for the scoped REST/WS surface. Active the
    /// moment this `GatewayWelcome` is sent — the gateway only seals it after
    /// both the phone user and the operator confirmed the pairing.
    pub auth_token: String,
}

/// App → gateway, as a Noise transport message: the phone user's pairing
/// decision after comparing the displayed confirmation code (see
/// [`crate::kdf::derive_confirm_code`]). Inside the authenticated Noise channel
/// a blind relay can neither forge an acceptance nor flip a decline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConfirm {
    pub accepted: bool,
}

/// On-wire envelope for the device-pairing handshake over the relay pairing
/// WebSocket (each variant is one binary frame, MessagePack-encoded). See the
/// module docs for the message sequence.
///
/// `msg` payloads are opaque Noise bytes: handshake messages for
/// [`Hello`](Self::Hello) / [`HandshakeReply`](Self::HandshakeReply) /
/// [`HandshakeFinal`](Self::HandshakeFinal), and transport messages for
/// [`Sealed`](Self::Sealed). [`Reject`](Self::Reject) is a plaintext abort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PairFrame {
    /// P → A: the public rendezvous id (the gateway claims the in-flight slot by
    /// it, and it is bound into the handshake prologue) plus the app's `msg1`.
    Hello { rendezvous_id: String, msg: Vec<u8> },
    /// A → P: the gateway's `msg2`.
    HandshakeReply { msg: Vec<u8> },
    /// P → A: the app's `msg3` (Noise payload = the `DeviceHello` body).
    HandshakeFinal { msg: Vec<u8> },
    /// A Noise transport message (`DeviceConfirm` up, `GatewayWelcome` down).
    Sealed { msg: Vec<u8> },
    /// Either side aborting the handshake, in the clear.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kdf::{derive_confirm_code, derive_push_key};
    use crate::noise::StaticKeypair;
    use crate::psk_pair::{PairingSecret, PskHandshake, build_prologue};

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
            apns_token: "abc123".into(),
            apns_env: ApnsEnv::Sandbox,
        };
        let decoded: DeviceHello = decode(&encode(&hello).unwrap()).unwrap();
        assert_eq!(hello, decoded);
    }

    #[test]
    fn gateway_welcome_round_trips() {
        let welcome = GatewayWelcome {
            relay_node_id: "node-xyz".into(),
            relay_url: "wss://proxy.baybo.space".into(),
            user_id: "user-1".into(),
            rendezvous_id: "11111111-2222-4333-8444-555555555555".into(),
            auth_token: "deadbeef".into(),
        };
        let decoded: GatewayWelcome = decode(&encode(&welcome).unwrap()).unwrap();
        assert_eq!(welcome, decoded);
    }

    #[test]
    fn pair_frames_round_trip() {
        for frame in [
            PairFrame::Hello {
                rendezvous_id: "rid".into(),
                msg: vec![1, 2, 3],
            },
            PairFrame::HandshakeReply { msg: vec![4, 5] },
            PairFrame::HandshakeFinal { msg: vec![6, 7] },
            PairFrame::Sealed { msg: vec![9; 40] },
            PairFrame::Reject {
                reason: "expired rendezvous".into(),
            },
        ] {
            let decoded: PairFrame = decode(&encode(&frame).unwrap()).unwrap();
            assert_eq!(frame, decoded);
        }
    }

    /// End-to-end: the full XXpsk0 handshake + the sealed pairing exchange both
    /// ways — exactly the pairing wire path (minus the rendezvous relay). Proves
    /// both ends agree on the confirm code + push key and recover each message.
    #[test]
    fn sealed_exchange_over_pairing_channel() {
        let secret = PairingSecret::generate();
        let app_kp = StaticKeypair::generate().unwrap();
        let gw_kp = StaticKeypair::generate().unwrap();
        let prologue = build_prologue("rid", "wss://relay");

        let mut app = PskHandshake::start_initiator(&app_kp.secret(), &secret, &prologue).unwrap();
        let mut gw = PskHandshake::start_responder(&gw_kp.secret(), &secret, &prologue).unwrap();

        // msg1 / msg2.
        let msg1 = app.write_handshake(&[]).unwrap();
        gw.read_handshake(&msg1).unwrap();
        let msg2 = gw.write_handshake(&[]).unwrap();
        app.read_handshake(&msg2).unwrap();

        // msg3 carries the DeviceHello body.
        let hello = DeviceHello {
            device_id: "dev-1".into(),
            apns_token: "tok".into(),
            apns_env: ApnsEnv::Production,
        };
        let msg3 = app.write_handshake(&encode(&hello).unwrap()).unwrap();
        let got_hello: DeviceHello = decode(&gw.read_handshake(&msg3).unwrap()).unwrap();
        assert_eq!(got_hello, hello);

        let mut app_t = app.into_transport().unwrap();
        let mut gw_t = gw.into_transport().unwrap();

        // Channel binding agrees on both ends.
        assert_eq!(app_t.handshake_hash(), gw_t.handshake_hash());
        assert_eq!(
            derive_confirm_code(app_t.handshake_hash()).unwrap(),
            derive_confirm_code(gw_t.handshake_hash()).unwrap(),
        );
        assert_eq!(
            derive_push_key(app_t.handshake_hash()).unwrap(),
            derive_push_key(gw_t.handshake_hash()).unwrap(),
        );
        // Each side learned the other's real static in-band.
        assert_eq!(app_t.remote_static(), &gw_kp.public());
        assert_eq!(gw_t.remote_static(), &app_kp.public());

        // P → A DeviceConfirm.
        let confirm = encode(&DeviceConfirm { accepted: true }).unwrap();
        let sealed = app_t.write(&confirm).unwrap();
        let got: DeviceConfirm = decode(&gw_t.read(&sealed).unwrap()).unwrap();
        assert!(got.accepted);

        // A → P GatewayWelcome.
        let welcome = GatewayWelcome {
            relay_node_id: "n1".into(),
            relay_url: String::new(),
            user_id: "u1".into(),
            rendezvous_id: "rid".into(),
            auth_token: "tok".into(),
        };
        let sealed = gw_t.write(&encode(&welcome).unwrap()).unwrap();
        let got: GatewayWelcome = decode(&app_t.read(&sealed).unwrap()).unwrap();
        assert_eq!(got, welcome);
    }
}
