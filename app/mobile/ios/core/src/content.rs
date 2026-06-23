//! The app side of the E2E **content session**: a Noise channel over which the
//! app self-pulls a thread.
//!
//! After pairing the app holds its own Noise static key and the gateway's. On
//! open it runs a Noise **IK** handshake as the initiator (it already knows A's
//! static key, so it's near-0-RTT), then exchanges `Frame`s inside the
//! authenticated, forward-secret transport: it sends a
//! [`Frame::Subscribe`](wire::Frame::Subscribe) and decodes the replayed
//! [`Frame::Message`](wire::Frame::Message) rows. C (the relay) sees only
//! Noise ciphertext.
//!
//! Transport-agnostic and host-testable: the Tauri shell pumps the opaque bytes
//! over the direct or relayed WebSocket; the crypto is entirely
//! [`device_proto`] + the `Frame` codec is [`wire`], so interop with
//! the gateway is guaranteed by construction.

use device_proto::noise::StaticKeypair;
use wire::{Frame, decode, encode};
use snow::{HandshakeState, TransportState};

use crate::error::MobileError;

/// snow's per-message ceiling; Noise frames cannot exceed this.
const MAX_NOISE_MSG: usize = 65535;

/// An in-progress Noise IK handshake (initiator = the app).
pub struct ContentHandshake {
    state: HandshakeState,
}

impl ContentHandshake {
    /// Begin the handshake. Returns the first message to send to the gateway.
    pub fn start(
        local: &StaticKeypair,
        gateway_static: &[u8; 32],
    ) -> Result<(Self, Vec<u8>), MobileError> {
        let mut state = local.ik_initiator(gateway_static)?;
        let mut buf = vec![0u8; MAX_NOISE_MSG];
        let n = state.write_message(&[], &mut buf)?;
        buf.truncate(n);
        Ok((Self { state }, buf))
    }

    /// Process the gateway's handshake reply and finalize the session.
    pub fn finish(mut self, reply: &[u8]) -> Result<ContentSession, MobileError> {
        let mut buf = vec![0u8; MAX_NOISE_MSG];
        self.state.read_message(reply, &mut buf)?;
        Ok(ContentSession {
            transport: self.state.into_transport_mode()?,
        })
    }
}

/// An established E2E content session. Seal `Frame`s to send and open received
/// ones, all inside Noise.
pub struct ContentSession {
    transport: TransportState,
}

impl ContentSession {
    /// Seal a `Frame` for transmission (e.g. a `Frame::Subscribe` catch-up
    /// request). Returns the opaque ciphertext to send.
    pub fn seal(&mut self, frame: &Frame) -> Result<Vec<u8>, MobileError> {
        let plaintext = encode(frame)?;
        let mut buf = vec![0u8; plaintext.len() + 16];
        let n = self.transport.write_message(&plaintext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Open a received ciphertext into a `Frame` (e.g. a replayed
    /// `Frame::Message`).
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Frame, MobileError> {
        let mut buf = vec![0u8; ciphertext.len()];
        let n = self.transport.read_message(ciphertext, &mut buf)?;
        buf.truncate(n);
        Ok(decode(&buf)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{ChannelType, SessionId};
    use wire::{Message, MessageRole};

    /// The app's content session round-trips a self-pull against an in-process
    /// gateway (IK responder built from the same `device-proto`): the app
    /// subscribes, the gateway replays a `Frame::Message`, the app decodes it.
    #[test]
    fn self_pull_round_trips_over_noise() {
        let app = StaticKeypair::generate().unwrap();
        let gw = StaticKeypair::generate().unwrap();

        // App (initiator) starts; gateway (responder) completes the IK handshake.
        let (handshake, msg1) = ContentHandshake::start(&app, &gw.public()).unwrap();
        let mut gw_hs = gw.ik_responder().unwrap();
        let mut buf = [0u8; MAX_NOISE_MSG];
        gw_hs.read_message(&msg1, &mut buf).unwrap();
        let n = gw_hs.write_message(&[], &mut buf).unwrap();
        let mut session = handshake.finish(&buf[..n]).unwrap();
        let mut gw_t = gw_hs.into_transport_mode().unwrap();

        // The gateway authenticated the app's static key.
        // (Initiator's static is learned by the IK responder during msg1.)

        // App → gateway: Subscribe for catch-up.
        let subscribe = Frame::Subscribe {
            session_id: SessionId::from("sess-1"),
            since_ordinal: Some(41),
        };
        let sealed = session.seal(&subscribe).unwrap();
        let mut gw_in = [0u8; MAX_NOISE_MSG];
        let m = gw_t.read_message(&sealed, &mut gw_in).unwrap();
        let got = decode(&gw_in[..m]).unwrap();
        assert_eq!(got, subscribe);

        // Gateway → app: a replayed assistant message; the app decodes it.
        let reply = Frame::Message(Message {
            content: "the agent's full reply".into(),
            session_id: SessionId::from("sess-1"),
            user_id: "u1".into(),
            channel_type: ChannelType::ios(),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
            role: MessageRole::Assistant,
            ordinal: Some(42),
        });
        let reply_bytes = encode(&reply).unwrap();
        let mut out = [0u8; MAX_NOISE_MSG];
        let r = gw_t.write_message(&reply_bytes, &mut out).unwrap();
        let decoded = session.open(&out[..r]).unwrap();
        assert_eq!(decoded, reply);
    }

    #[test]
    fn wrong_gateway_static_key_fails_handshake() {
        let app = StaticKeypair::generate().unwrap();
        let real_gw = StaticKeypair::generate().unwrap();
        let wrong = StaticKeypair::generate().unwrap();

        // App initiates toward the WRONG static key; the real gateway can't
        // complete the handshake (IK authenticates the responder's static).
        let (_hs, msg1) = ContentHandshake::start(&app, &wrong.public()).unwrap();
        let mut gw_hs = real_gw.ik_responder().unwrap();
        let mut buf = [0u8; MAX_NOISE_MSG];
        assert!(gw_hs.read_message(&msg1, &mut buf).is_err());
    }
}
