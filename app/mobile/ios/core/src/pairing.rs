//! The app (P) side of the SPAKE2 device-pairing handshake.
//!
//! A transport-agnostic state machine: the Tauri shell pumps
//! [`PairFrame`]s over the `/v1/device/pair` WebSocket (direct or via C's
//! rendezvous) and feeds them through these methods. The crypto is entirely
//! [`aura_device_proto`], so the app and the gateway agree by construction.
//!
//! Flow: [`PairingClient::start`] → send `Hello` → [`on_pake_reply`] → send the
//! sealed `DeviceHello` → [`on_welcome`] → [`PairedGateway`].
//!
//! [`on_pake_reply`]: PairingClient::on_pake_reply
//! [`on_welcome`]: PairingClient::on_welcome

use aura_device_proto::aead::KEY_LEN;
use aura_device_proto::kdf::{PairKeys, derive_pair_keys};
use aura_device_proto::pairing::{self, ApnsEnv, DeviceHello, GatewayWelcome, PairFrame};
use aura_device_proto::pake::Pake;

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

/// The result of a completed (still operator-pending) pairing — everything the
/// app persists to talk to this gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedGateway {
    pub user_id: String,
    /// Bearer token for the scoped REST/WS surface (inert until approved).
    pub auth_token: String,
    /// The gateway's Noise static public key (authenticates every later session).
    pub gateway_static_pubkey: [u8; KEY_LEN],
    /// The per-device push key (HKDF of the SPAKE2 secret) — stored in the
    /// shared Keychain for the NSE.
    pub push_key: [u8; KEY_LEN],
    pub relay_node_id: String,
    pub direct_candidates: Vec<String>,
    /// The retained code (the operator's approval handle).
    pub pairing_code: String,
}

/// App-side pairing state machine.
pub struct PairingClient {
    req: PairingRequest,
    pake: Option<Pake>,
    keys: Option<PairKeys>,
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
            },
            hello,
        )
    }

    /// On the gateway's `PakeReply`: derive the shared keys and return the
    /// sealed [`DeviceHello`] to send.
    pub fn on_pake_reply(&mut self, gateway_pake: &[u8]) -> Result<PairFrame, MobileError> {
        let pake = self
            .pake
            .take()
            .ok_or(MobileError::State("pake already consumed"))?;
        let k = pake.finish(gateway_pake)?;
        let keys = derive_pair_keys(&k)?;
        let hello = DeviceHello {
            device_id: self.req.device_id.clone(),
            label: self.req.label.clone(),
            static_pubkey: self.req.static_pubkey,
            apns_token: self.req.apns_token.clone(),
            apns_env: self.req.apns_env,
        };
        let (nonce, ciphertext) = pairing::seal_msg(&keys.channel_key, &hello)?;
        self.keys = Some(keys);
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
            direct_candidates: welcome.direct_candidates,
            pairing_code: welcome.pairing_code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the full 4-message handshake in-process: the client against an
    /// in-process gateway side built from the same `aura-device-proto`. Proves
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
        let PairFrame::Hello { code, pake: app_pake } = hello else {
            panic!("expected Hello");
        };
        assert_eq!(code, "WORMHOLE-7-foo");
        let (gw_pake_state, gw_pake) = Pake::start_gateway(&code);
        let gw_k = gw_pake_state.finish(&app_pake).unwrap();
        let gw_keys = derive_pair_keys(&gw_k).unwrap();

        // client processes PakeReply → sealed DeviceHello
        let PairFrame::Sealed { nonce, ciphertext } =
            client.on_pake_reply(&gw_pake).unwrap()
        else {
            panic!("expected sealed DeviceHello");
        };
        let device_hello: DeviceHello =
            pairing::open_msg(&gw_keys.channel_key, &nonce, &ciphertext).unwrap();
        assert_eq!(device_hello.device_id, "dev-xyz");
        assert_eq!(device_hello.static_pubkey, [4u8; KEY_LEN]);

        // gateway seals GatewayWelcome (with the issued token + its static key)
        let welcome = GatewayWelcome {
            static_pubkey: [9u8; KEY_LEN],
            relay_node_id: "node-1".into(),
            direct_candidates: vec!["wss://aura.lan:8889".into()],
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
        assert_eq!(paired.direct_candidates, vec!["wss://aura.lan:8889".to_string()]);
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
