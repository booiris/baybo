//! App side of the XXpsk0 device-pairing handshake.

use device_proto::aead::KEY_LEN;
use device_proto::kdf::{derive_confirm_code, derive_push_key};
use device_proto::pairing::{
    self, DeviceConfirm, DeviceDelegation, DeviceHello, GatewayWelcome, PairFrame,
};
use device_proto::psk_pair::{PairingSecret, PskHandshake, PskTransport, build_prologue};

use super::error::MobileError;

pub struct PairingRequest {
    pub rendezvous_id: String,
    pub secret: PairingSecret,
    pub endpoint: String,
    pub device_id: String,
    pub static_secret: [u8; KEY_LEN],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedGateway {
    pub auth_token: String,
    pub gateway_static_pubkey: [u8; KEY_LEN],
    pub push_key: [u8; KEY_LEN],
    pub relay_node_id: String,
    pub relay_url: String,
    pub rendezvous_id: String,
    pub gateway_push_pubkey: Option<[u8; KEY_LEN]>,
}

#[derive(Debug, Clone)]
pub struct PairChallenge {
    pub device_id: String,
    pub confirm_code: String,
}

#[derive(Debug, Clone)]
pub struct PairedSummary {
    pub relay_node_id: String,
    pub rendezvous_id: String,
}

impl From<&PairedGateway> for PairedSummary {
    fn from(p: &PairedGateway) -> Self {
        Self {
            relay_node_id: p.relay_node_id.clone(),
            rendezvous_id: p.rendezvous_id.clone(),
        }
    }
}

pub struct PairingClient {
    hello: DeviceHello,
    handshake: Option<PskHandshake>,
    transport: Option<PskTransport>,
    confirm_code: Option<String>,
}

impl PairingClient {
    pub fn start(req: PairingRequest) -> Result<(Self, PairFrame), MobileError> {
        let prologue = build_prologue(&req.rendezvous_id, &req.endpoint);
        let mut handshake =
            PskHandshake::start_initiator(&req.static_secret, &req.secret, &prologue)?;
        let msg1 = handshake.write_handshake(&[])?;
        let hello = DeviceHello {
            device_id: req.device_id,
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

    pub fn confirm_code(&self) -> Option<&str> {
        self.confirm_code.as_deref()
    }

    pub fn confirm(&mut self, accepted: bool) -> Result<PairFrame, MobileError> {
        let transport = self
            .transport
            .as_mut()
            .ok_or(MobileError::State("transport not ready"))?;
        let msg = transport.write(&pairing::encode(&DeviceConfirm { accepted })?)?;
        Ok(PairFrame::Sealed { msg })
    }

    pub fn on_welcome(&mut self, msg: &[u8]) -> Result<PairedGateway, MobileError> {
        let transport = self
            .transport
            .as_mut()
            .ok_or(MobileError::State("transport not ready"))?;
        let welcome: GatewayWelcome = pairing::decode(&transport.read(msg)?)?;
        let push_key = derive_push_key(transport.handshake_hash())?;
        let gateway_static_pubkey = *transport.remote_static();
        let gateway_push_pubkey = (welcome.gateway_push_pubkey.len() == KEY_LEN).then(|| {
            let mut k = [0u8; KEY_LEN];
            k.copy_from_slice(&welcome.gateway_push_pubkey);
            k
        });
        Ok(PairedGateway {
            auth_token: welcome.auth_token,
            gateway_static_pubkey,
            push_key,
            relay_node_id: welcome.relay_node_id,
            relay_url: welcome.relay_url,
            rendezvous_id: welcome.rendezvous_id,
            gateway_push_pubkey,
        })
    }

    pub fn seal_delegation(&mut self, delegation: Vec<u8>) -> Result<PairFrame, MobileError> {
        let transport = self
            .transport
            .as_mut()
            .ok_or(MobileError::State("transport not ready"))?;
        let msg = transport.write(&pairing::encode(&DeviceDelegation { delegation })?)?;
        Ok(PairFrame::Sealed { msg })
    }
}
