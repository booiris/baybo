//! The app (P) side of the SPAKE2 device-pairing handshake.
//!
//! A transport-agnostic state machine: the Tauri shell pumps
//! [`PairFrame`]s over the `/v1/device/pair` WebSocket (direct or via C's
//! rendezvous) and feeds them through these methods. The crypto is entirely
//! [`device_proto`], so the app and the gateway agree by construction.
//!
//! Flow: [`PairingClient::start`] → send `Hello` → [`on_pake_reply`] → send the
//! sealed `DeviceHello` → show [`confirm_code`], [`confirm`] → send the sealed
//! `DeviceConfirm` → [`on_welcome`] → [`PairedGateway`].
//!
//! [`on_pake_reply`]: PairingClient::on_pake_reply
//! [`confirm_code`]: PairingClient::confirm_code
//! [`confirm`]: PairingClient::confirm
//! [`on_welcome`]: PairingClient::on_welcome

use device_proto::aead::KEY_LEN;
use device_proto::kdf::{PairKeys, derive_confirm_code, derive_pair_keys};
use device_proto::pairing::{self, ApnsEnv, DeviceConfirm, DeviceHello, GatewayWelcome, PairFrame};
use device_proto::pake::Pake;
use serde::Serialize;

use crate::error::MobileError;

/// What the app supplies to begin pairing (scanned code + its own identity +
/// push registration).
pub struct PairingRequest {
    /// The short code from the QR.
    pub code: String,
    /// Client-generated, per-pairing device id.
    pub device_id: String,
    /// Human label ("Booiris iPhone").
    pub label: String,
    /// The app's long-term Noise static **public** key (the secret stays in the
    /// app's keystore, outside this handshake).
    pub static_pubkey: [u8; KEY_LEN],
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
    /// The gateway's Noise static public key (authenticates every later session).
    pub gateway_static_pubkey: [u8; KEY_LEN],
    /// The per-device push key (HKDF of the SPAKE2 secret) — stored in the
    /// shared Keychain for the NSE.
    pub push_key: [u8; KEY_LEN],
    pub relay_node_id: String,
    /// Base WS URL of the blind relay (C); the app dials the relay's content-join
    /// route (`remote_host_protocol::relay::content_join_url`) to reach a NAT'd
    /// gateway when every direct candidate fails. Empty when relay is off.
    pub relay_url: String,
    pub direct_candidates: Vec<String>,
    /// The code this device paired under (retained for audit).
    pub pairing_code: String,
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
/// shell, never returned to the webview.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../src/generated/"))]
#[serde(rename_all = "camelCase")]
pub struct PairedSummary {
    pub user_id: String,
    pub relay_node_id: String,
    pub direct_candidates: Vec<String>,
    pub pairing_code: String,
}

impl From<&PairedGateway> for PairedSummary {
    fn from(p: &PairedGateway) -> Self {
        Self {
            user_id: p.user_id.clone(),
            relay_node_id: p.relay_node_id.clone(),
            direct_candidates: p.direct_candidates.clone(),
            pairing_code: p.pairing_code.clone(),
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
    req: PairingRequest,
    pake: Option<Pake>,
    keys: Option<PairKeys>,
    /// The confirmation code to show the user, derived from K in `on_pake_reply`.
    confirm_code: Option<String>,
}

impl PairingClient {
    /// Begin pairing. Returns the [`PairFrame::Hello`] to send first.
    pub fn start(req: PairingRequest) -> (Self, PairFrame) {
        let (pake, app_msg) = Pake::start_app(&req.code);
        let hello = PairFrame::Hello {
            code: req.code.clone(),
            pake: app_msg,
        };
        (
            Self {
                req,
                pake: Some(pake),
                keys: None,
                confirm_code: None,
            },
            hello,
        )
    }

    /// On the gateway's `PakeReply`: derive the shared keys + the confirmation
    /// code and return the sealed [`DeviceHello`] to send.
    pub fn on_pake_reply(&mut self, gateway_pake: &[u8]) -> Result<PairFrame, MobileError> {
        let pake = self
            .pake
            .take()
            .ok_or(MobileError::State("pake already consumed"))?;
        let k = pake.finish(gateway_pake)?;
        let keys = derive_pair_keys(&k)?;
        let confirm_code = derive_confirm_code(&k)?;
        let hello = DeviceHello {
            device_id: self.req.device_id.clone(),
            label: self.req.label.clone(),
            static_pubkey: self.req.static_pubkey,
            apns_token: self.req.apns_token.clone(),
            apns_env: self.req.apns_env,
        };
        let (nonce, ciphertext) = pairing::seal_msg(&keys.channel_key, &hello)?;
        self.keys = Some(keys);
        self.confirm_code = Some(confirm_code);
        Ok(PairFrame::Sealed { nonce, ciphertext })
    }

    /// The Bluetooth-style confirmation code to show the user (available after
    /// [`on_pake_reply`](Self::on_pake_reply)). The operator sees the same
    /// number on their terminal; the user pairs only if they match.
    pub fn confirm_code(&self) -> Option<&str> {
        self.confirm_code.as_deref()
    }

    /// The user's pairing decision after comparing the confirmation code.
    /// Returns the sealed [`DeviceConfirm`] frame to send; on `false` the
    /// gateway aborts the handshake.
    pub fn confirm(&self, accepted: bool) -> Result<PairFrame, MobileError> {
        let keys = self
            .keys
            .as_ref()
            .ok_or(MobileError::State("keys not derived yet"))?;
        let (nonce, ciphertext) =
            pairing::seal_msg(&keys.channel_key, &DeviceConfirm { accepted })?;
        Ok(PairFrame::Sealed { nonce, ciphertext })
    }

    /// On the gateway's sealed `GatewayWelcome`: open it into a [`PairedGateway`].
    pub fn on_welcome(
        &self,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<PairedGateway, MobileError> {
        let keys = self
            .keys
            .as_ref()
            .ok_or(MobileError::State("keys not derived yet"))?;
        let welcome: GatewayWelcome = pairing::open_msg(&keys.channel_key, nonce, ciphertext)?;
        Ok(PairedGateway {
            user_id: welcome.user_id,
            auth_token: welcome.auth_token,
            gateway_static_pubkey: welcome.static_pubkey,
            push_key: keys.push_key,
            relay_node_id: welcome.relay_node_id,
            relay_url: welcome.relay_url,
            direct_candidates: welcome.direct_candidates,
            pairing_code: welcome.pairing_code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the full 4-message handshake in-process: the client against an
    /// in-process gateway side built from the same `device-proto`. Proves
    /// the app derives the same push key and recovers the welcome — the same
    /// interop the real gateway's e2e test exercises, from the app's side.
    #[test]
    fn full_handshake_yields_consistent_paired_gateway() {
        let req = PairingRequest {
            code: "WORMHOLE-7-foo".into(),
            device_id: "dev-xyz".into(),
            label: "Test iPhone".into(),
            static_pubkey: [4u8; KEY_LEN],
            apns_token: "apns-tok".into(),
            apns_env: ApnsEnv::Sandbox,
        };
        let (mut client, hello) = PairingClient::start(req);

        // --- gateway side (mirrors the /v1/device/pair route) ---
        let PairFrame::Hello {
            code,
            pake: app_pake,
        } = hello
        else {
            panic!("expected Hello");
        };
        assert_eq!(code, "WORMHOLE-7-foo");
        let (gw_pake_state, gw_pake) = Pake::start_gateway(&code);
        let gw_k = gw_pake_state.finish(&app_pake).unwrap();
        let gw_keys = derive_pair_keys(&gw_k).unwrap();
        let gw_confirm = derive_confirm_code(&gw_k).unwrap();

        // client processes PakeReply → sealed DeviceHello
        let PairFrame::Sealed { nonce, ciphertext } = client.on_pake_reply(&gw_pake).unwrap()
        else {
            panic!("expected sealed DeviceHello");
        };
        let device_hello: DeviceHello =
            pairing::open_msg(&gw_keys.channel_key, &nonce, &ciphertext).unwrap();
        assert_eq!(device_hello.device_id, "dev-xyz");
        assert_eq!(device_hello.static_pubkey, [4u8; KEY_LEN]);

        // both ends derived the same confirmation code (numeric comparison)
        assert_eq!(client.confirm_code(), Some(gw_confirm.as_str()));

        // user accepts → client seals DeviceConfirm; gateway opens it
        let PairFrame::Sealed {
            nonce: cn,
            ciphertext: cc,
        } = client.confirm(true).unwrap()
        else {
            panic!("expected sealed DeviceConfirm");
        };
        let device_confirm: DeviceConfirm =
            pairing::open_msg(&gw_keys.channel_key, &cn, &cc).unwrap();
        assert!(device_confirm.accepted);

        // gateway seals GatewayWelcome (with the issued token + its static key)
        let welcome = GatewayWelcome {
            static_pubkey: [9u8; KEY_LEN],
            relay_node_id: "node-1".into(),
            relay_url: "wss://proxy.baybo.space".into(),
            direct_candidates: vec!["wss://baybo.lan:8889".into()],
            user_id: "user-1".into(),
            pairing_code: code.clone(),
            auth_token: "issued-token".into(),
        };
        let (wn, wc) = pairing::seal_msg(&gw_keys.channel_key, &welcome).unwrap();

        // client opens the welcome
        let paired = client.on_welcome(&wn, &wc).unwrap();
        assert_eq!(paired.user_id, "user-1");
        assert_eq!(paired.auth_token, "issued-token");
        assert_eq!(paired.gateway_static_pubkey, [9u8; KEY_LEN]);
        assert_eq!(paired.relay_node_id, "node-1");
        assert_eq!(
            paired.direct_candidates,
            vec!["wss://baybo.lan:8889".to_string()]
        );
        // Both ends derived the same push key from the SPAKE2 secret.
        assert_eq!(paired.push_key, gw_keys.push_key);
    }

    #[test]
    fn welcome_before_pake_reply_is_a_state_error() {
        let req = PairingRequest {
            code: "c".into(),
            device_id: "d".into(),
            label: "l".into(),
            static_pubkey: [0u8; KEY_LEN],
            apns_token: "t".into(),
            apns_env: ApnsEnv::Sandbox,
        };
        let (client, _hello) = PairingClient::start(req);
        assert!(matches!(
            client.on_welcome(&[0u8; 12], &[]),
            Err(MobileError::State(_)),
        ));
    }
}
