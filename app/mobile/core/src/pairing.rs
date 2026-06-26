//! The app (P) side of the XXpsk0 device-pairing handshake.
//!
//! A transport-agnostic state machine: the Tauri shell pumps [`PairFrame`]s over
//! the relay pairing WebSocket (C's `/pair/join` rendezvous) and feeds them
//! through these methods. The crypto is entirely [`device_proto`], so the app and
//! the gateway agree by construction.
//!
//! Flow: [`PairingClient::start`] → send `Hello` →
//! [`on_handshake_reply`] → send `HandshakeFinal` (whose Noise payload is the
//! `DeviceHello` body) → show [`confirm_code`], [`confirm`] → send the sealed
//! `DeviceConfirm` → [`on_welcome`] → [`PairedGateway`].
//!
//! The app's security against a malicious relay comes from the QR **secret**: it
//! is the Noise PSK ([`PairingRequest::secret`]), carried out of band and never
//! over the relay. The gateway's static key is learned **in-band** as an XX
//! handshake token (not from the QR, not from the welcome), so the relay cannot
//! substitute its own.
//!
//! [`on_handshake_reply`]: PairingClient::on_handshake_reply
//! [`confirm_code`]: PairingClient::confirm_code
//! [`confirm`]: PairingClient::confirm
//! [`on_welcome`]: PairingClient::on_welcome

use device_proto::aead::KEY_LEN;
use device_proto::kdf::{derive_confirm_code, derive_push_key};
use device_proto::pairing::{self, ApnsEnv, DeviceConfirm, DeviceHello, GatewayWelcome, PairFrame};
use device_proto::psk_pair::{PairingSecret, PskHandshake, PskTransport, build_prologue};
use serde::Serialize;

use crate::error::MobileError;

/// What the app supplies to begin pairing (scanned rendezvous id + secret + its
/// own identity + push registration).
pub struct PairingRequest {
    /// The public rendezvous id from the QR (`r=`). Routes the rendezvous and is
    /// bound into the handshake prologue.
    pub rendezvous_id: String,
    /// The 256-bit secret from the QR (`s=`), used as the Noise PSK. Never sent
    /// over the relay.
    pub secret: PairingSecret,
    /// The relay endpoint from the QR (`h=`). Bound into the handshake prologue,
    /// so it must match the gateway's configured relay URL (anti-cross-binding).
    pub endpoint: String,
    /// Client-generated, per-pairing device id.
    pub device_id: String,
    /// The app's long-term Noise static **secret**. It rides the handshake as an
    /// XX token (the gateway learns the public half in-band); it never leaves the
    /// keystore otherwise.
    pub static_secret: [u8; KEY_LEN],
    /// APNs device token (gateway-mediated registration relays it to C).
    pub apns_token: String,
    pub apns_env: ApnsEnv,
}

/// The result of a completed pairing — everything the app persists to talk to
/// this gateway. Only produced once both sides confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedGateway {
    pub user_id: String,
    /// Bearer token for the scoped REST/WS surface. Active — the gateway only
    /// seals the welcome after the phone user and the operator both confirmed.
    pub auth_token: String,
    /// The gateway's Noise static public key, learned in-band as an XX token —
    /// authenticates every later session.
    pub gateway_static_pubkey: [u8; KEY_LEN],
    /// The per-device push key (HKDF of the Noise handshake hash `h`) — stored in
    /// the shared Keychain for the NSE.
    pub push_key: [u8; KEY_LEN],
    pub relay_node_id: String,
    /// Base WS URL of the blind relay (C); the app dials the relay's content-join
    /// route (`remote_host_protocol::relay::content_join_url`) to reach the
    /// (possibly NAT'd) gateway for content. Empty when relay is off.
    pub relay_url: String,
    /// The public rendezvous id this device paired under (retained for audit).
    pub rendezvous_id: String,
}

/// What `pair_begin` returns (a Tauri IPC DTO): the confirmation code to show
/// the user + the device id the UI passes back to `pair_confirm`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../src/generated/"))]
#[serde(rename_all = "camelCase")]
pub struct PairChallenge {
    pub device_id: String,
    pub confirm_code: String,
}

/// What the UI renders after a successful pairing (a Tauri IPC DTO). The secrets
/// (`auth_token`, `push_key`, the Noise static secret) are persisted by the
/// shell, never returned to the webview — and the QR secret is never carried at
/// all (only the public `rendezvous_id`).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../src/generated/"))]
#[serde(rename_all = "camelCase")]
pub struct PairedSummary {
    pub user_id: String,
    pub relay_node_id: String,
    pub rendezvous_id: String,
}

impl From<&PairedGateway> for PairedSummary {
    fn from(p: &PairedGateway) -> Self {
        Self {
            user_id: p.user_id.clone(),
            relay_node_id: p.relay_node_id.clone(),
            rendezvous_id: p.rendezvous_id.clone(),
        }
    }
}

/// Pushed to the UI (over a Tauri channel) when the gateway aborts pairing —
/// the operator declined, or the connection dropped — *before* the phone user
/// has tapped Pair/Cancel. Lets the confirm-code screen dismiss itself instead
/// of hanging on a code the other side has already abandoned. A DTO, so it is
/// generated into the webview's TS bindings by ts-rs.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../src/generated/"))]
#[serde(rename_all = "camelCase")]
pub struct PairAborted {
    /// A short, human-readable reason (the gateway's reject text, or why the
    /// connection ended). Shown to the user as context, not parsed.
    pub reason: String,
}

/// App-side pairing state machine.
pub struct PairingClient {
    /// The `DeviceHello` body, sent as the Noise payload of `msg3`.
    hello: DeviceHello,
    /// The in-flight handshake (until `on_handshake_reply` finishes it).
    handshake: Option<PskHandshake>,
    /// The completed session (transport mode + channel binding), set once the
    /// handshake finishes.
    transport: Option<PskTransport>,
    /// The confirmation code to show the user, derived from `h` once the
    /// handshake finishes.
    confirm_code: Option<String>,
}

impl PairingClient {
    /// Begin pairing. Returns the [`PairFrame::Hello`] to send first (the public
    /// rendezvous id + the app's `msg1`).
    pub fn start(req: PairingRequest) -> Result<(Self, PairFrame), MobileError> {
        let prologue = build_prologue(&req.rendezvous_id, &req.endpoint);
        let mut handshake =
            PskHandshake::start_initiator(&req.static_secret, &req.secret, &prologue)?;
        let msg1 = handshake.write_handshake(&[])?;
        let hello = DeviceHello {
            device_id: req.device_id,
            apns_token: req.apns_token,
            apns_env: req.apns_env,
        };
        let frame = PairFrame::Hello {
            rendezvous_id: req.rendezvous_id,
            msg: msg1,
        };
        Ok((
            Self {
                hello,
                handshake: Some(handshake),
                transport: None,
                confirm_code: None,
            },
            frame,
        ))
    }

    /// On the gateway's `HandshakeReply` (`msg2`): read it (learning A's static),
    /// then return the `HandshakeFinal` to send (`msg3`, whose Noise payload is
    /// the `DeviceHello` body). Derives the confirmation code from `h`.
    pub fn on_handshake_reply(&mut self, reply: &[u8]) -> Result<PairFrame, MobileError> {
        let mut handshake = self
            .handshake
            .take()
            .ok_or(MobileError::State("handshake already consumed"))?;
        handshake.read_handshake(reply)?;
        let msg3 = handshake.write_handshake(&pairing::encode(&self.hello)?)?;
        let transport = handshake.into_transport()?;
        self.confirm_code = Some(derive_confirm_code(transport.handshake_hash())?);
        self.transport = Some(transport);
        Ok(PairFrame::HandshakeFinal { msg: msg3 })
    }

    /// The Bluetooth-style confirmation code to show the user (available after
    /// [`on_handshake_reply`](Self::on_handshake_reply)). The operator sees the
    /// same number on their terminal; the user pairs only if they match.
    pub fn confirm_code(&self) -> Option<&str> {
        self.confirm_code.as_deref()
    }

    /// The user's pairing decision after comparing the confirmation code.
    /// Returns the sealed [`DeviceConfirm`] transport frame to send; on `false`
    /// the gateway aborts the handshake.
    pub fn confirm(&mut self, accepted: bool) -> Result<PairFrame, MobileError> {
        let transport = self
            .transport
            .as_mut()
            .ok_or(MobileError::State("transport not ready"))?;
        let msg = transport.write(&pairing::encode(&DeviceConfirm { accepted })?)?;
        Ok(PairFrame::Sealed { msg })
    }

    /// On the gateway's sealed `GatewayWelcome` (a transport frame): open it into
    /// a [`PairedGateway`]. The gateway's static comes from the handshake (XX
    /// token), the push key from `h`.
    pub fn on_welcome(&mut self, msg: &[u8]) -> Result<PairedGateway, MobileError> {
        let transport = self
            .transport
            .as_mut()
            .ok_or(MobileError::State("transport not ready"))?;
        let welcome: GatewayWelcome = pairing::decode(&transport.read(msg)?)?;
        let push_key = derive_push_key(transport.handshake_hash())?;
        let gateway_static_pubkey = *transport.remote_static();
        Ok(PairedGateway {
            user_id: welcome.user_id,
            auth_token: welcome.auth_token,
            gateway_static_pubkey,
            push_key,
            relay_node_id: welcome.relay_node_id,
            relay_url: welcome.relay_url,
            rendezvous_id: welcome.rendezvous_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use device_proto::noise::StaticKeypair;

    const RID: &str = "11111111-2222-4333-8444-555555555555";
    const ENDPOINT: &str = "wss://proxy.baybo.space";

    /// Drive the full handshake in-process: the client against an in-process
    /// gateway side built from the same `device-proto`. Proves the app derives
    /// the same push key + confirm code and recovers the welcome — the interop
    /// the real gateway's e2e test exercises, from the app's side.
    #[test]
    fn full_handshake_yields_consistent_paired_gateway() {
        let secret = PairingSecret::generate();
        let app_static = StaticKeypair::generate().unwrap();
        let gw_static = StaticKeypair::generate().unwrap();

        let req = PairingRequest {
            rendezvous_id: RID.into(),
            secret: secret.clone(),
            endpoint: ENDPOINT.into(),
            device_id: "dev-xyz".into(),
            static_secret: app_static.secret(),
            apns_token: "apns-tok".into(),
            apns_env: ApnsEnv::Sandbox,
        };
        let (mut client, hello) = PairingClient::start(req).unwrap();

        // --- gateway side (mirrors `drive`) ---
        let PairFrame::Hello { rendezvous_id, msg } = hello else {
            panic!("expected Hello");
        };
        assert_eq!(rendezvous_id, RID);
        let prologue = build_prologue(RID, ENDPOINT);
        let mut gw =
            PskHandshake::start_responder(&gw_static.secret(), &secret, &prologue).unwrap();
        gw.read_handshake(&msg).unwrap();
        let msg2 = gw.write_handshake(&[]).unwrap();

        // client processes HandshakeReply → HandshakeFinal
        let PairFrame::HandshakeFinal { msg: msg3 } = client.on_handshake_reply(&msg2).unwrap()
        else {
            panic!("expected HandshakeFinal");
        };
        let device_hello: DeviceHello =
            pairing::decode(&gw.read_handshake(&msg3).unwrap()).unwrap();
        assert_eq!(device_hello.device_id, "dev-xyz");
        let mut gw_t = gw.into_transport().unwrap();

        // both ends derived the same confirmation code (numeric comparison)
        let gw_confirm = derive_confirm_code(gw_t.handshake_hash()).unwrap();
        assert_eq!(client.confirm_code(), Some(gw_confirm.as_str()));
        // the gateway learned the app's static in-band
        assert_eq!(gw_t.remote_static(), &app_static.public());

        // user accepts → client seals DeviceConfirm; gateway opens it
        let PairFrame::Sealed { msg: cc } = client.confirm(true).unwrap() else {
            panic!("expected sealed DeviceConfirm");
        };
        let device_confirm: DeviceConfirm = pairing::decode(&gw_t.read(&cc).unwrap()).unwrap();
        assert!(device_confirm.accepted);

        // gateway seals GatewayWelcome (with the issued token; its static is NOT
        // carried — the app already learned it as an XX token)
        let welcome = GatewayWelcome {
            relay_node_id: "node-1".into(),
            relay_url: "wss://proxy.baybo.space".into(),
            user_id: "user-1".into(),
            rendezvous_id: RID.into(),
            auth_token: "issued-token".into(),
        };
        let wc = gw_t.write(&pairing::encode(&welcome).unwrap()).unwrap();

        // client opens the welcome
        let paired = client.on_welcome(&wc).unwrap();
        assert_eq!(paired.user_id, "user-1");
        assert_eq!(paired.auth_token, "issued-token");
        assert_eq!(paired.gateway_static_pubkey, gw_static.public());
        assert_eq!(paired.relay_node_id, "node-1");
        assert_eq!(paired.rendezvous_id, RID);
        // Both ends derived the same push key from `h`.
        assert_eq!(
            paired.push_key,
            derive_push_key(gw_t.handshake_hash()).unwrap()
        );
    }

    #[test]
    fn welcome_before_handshake_reply_is_a_state_error() {
        let app_static = StaticKeypair::generate().unwrap();
        let req = PairingRequest {
            rendezvous_id: RID.into(),
            secret: PairingSecret::generate(),
            endpoint: ENDPOINT.into(),
            device_id: "d".into(),
            static_secret: app_static.secret(),
            apns_token: "t".into(),
            apns_env: ApnsEnv::Sandbox,
        };
        let (mut client, _hello) = PairingClient::start(req).unwrap();
        assert!(matches!(
            client.on_welcome(&[0u8; 48]),
            Err(MobileError::State(_)),
        ));
    }
}
