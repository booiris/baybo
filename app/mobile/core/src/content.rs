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
//! over the relay WebSocket; the crypto is entirely [`device_proto`] and the
//! `Frame` codec is [`wire`], so interop with the gateway is guaranteed by
//! construction.

use baybo_model::{ChannelType, SessionId};
use device_proto::noise::{FrameReassembler, NOISE_MAX_MESSAGE, StaticKeypair, write_chunked};
use snow::{HandshakeState, TransportState};
use wire::{Frame, Message, MessageRole, WireAttachment, decode, encode};

use crate::error::MobileError;

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
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = state.write_message(&[], &mut buf)?;
        buf.truncate(n);
        Ok((Self { state }, buf))
    }

    /// Process the gateway's handshake reply and finalize the session.
    pub fn finish(mut self, reply: &[u8]) -> Result<ContentSession, MobileError> {
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        self.state.read_message(reply, &mut buf)?;
        Ok(ContentSession {
            transport: self.state.into_transport_mode()?,
            reassembler: FrameReassembler::new(),
        })
    }

    /// Like [`finish`](Self::finish), but finalize into a [`BlobSession`](crate::blob::BlobSession)
    /// for a **blob leg** — the same Noise IK handshake, then the blob
    /// sub-protocol instead of the `Frame` loop. The leg is dialed with the
    /// `x-relay-leg-class: blob` header so the relay meters it as background.
    pub fn finish_blob(mut self, reply: &[u8]) -> Result<crate::blob::BlobSession, MobileError> {
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        self.state.read_message(reply, &mut buf)?;
        Ok(crate::blob::BlobSession::new(
            self.state.into_transport_mode()?,
        ))
    }
}

/// An established E2E content session. Seal `Frame`s to send and open received
/// ones, all inside Noise.
pub struct ContentSession {
    transport: TransportState,
    reassembler: FrameReassembler,
}

impl ContentSession {
    /// Seal a `Frame` into the Noise transport messages to send, chunked past the
    /// per-message ceiling (usually one message; a large frame yields several).
    pub fn seal(&mut self, frame: &Frame) -> Result<Vec<Vec<u8>>, MobileError> {
        let plaintext = encode(frame)?;
        Ok(write_chunked(&mut self.transport, &plaintext)?)
    }

    /// Open one received Noise message, returning every `Frame` it completes
    /// (none for a partial chunk, one, or several if the peer pipelined).
    pub fn open(&mut self, message: &[u8]) -> Result<Vec<Frame>, MobileError> {
        let mut frames = Vec::new();
        for bytes in self.reassembler.read(&mut self.transport, message)? {
            frames.push(decode(&bytes)?);
        }
        Ok(frames)
    }
}

/// A catch-up [`Frame::Subscribe`] for `session_id`. `since_ordinal` is the
/// highest ordinal the app has already rendered (so the gateway replays only
/// the gap); `None` is a fresh subscribe with no catch-up.
pub fn subscribe_frame(session_id: &str, since_ordinal: Option<i64>) -> Frame {
    Frame::Subscribe {
        session_id: SessionId::from(session_id),
        since_ordinal,
    }
}

/// An [`Frame::UpdateApnsToken`] telling the gateway the device's current APNs
/// token (and env, `"sandbox"`/`"production"`). Sent on every content connect so
/// the gateway keeps C's push binding fresh across APNs token rotation.
pub fn apns_token_frame(apns_token: &str, apns_env: &str) -> Frame {
    Frame::UpdateApnsToken {
        apns_token: apns_token.to_owned(),
        apns_env: apns_env.to_owned(),
    }
}

/// An outbound user [`Frame::Message`] on `session_id` carrying `attachments` —
/// content-addressed [`WireAttachment`] refs the app has already uploaded over a
/// blob leg (the bytes never ride this frame). `platform_msg_id` is a
/// client-chosen idempotency key (a fresh UUID per send) so a retry after a
/// transport blip doesn't double-fire the agent. `text` may be empty when the
/// message is attachment-only — the gateway drops the leading empty text block.
pub fn user_message_frame(
    session_id: &str,
    user_id: &str,
    text: &str,
    platform_msg_id: &str,
    attachments: Vec<WireAttachment>,
) -> Frame {
    Frame::Message(Message {
        content: text.to_owned(),
        session_id: SessionId::from(session_id),
        user_id: user_id.to_owned(),
        channel_type: ChannelType::ios(),
        bot_id: String::new(),
        attachments,
        platform_msg_id: platform_msg_id.to_owned(),
        role: MessageRole::User,
        ordinal: None,
    })
}

/// A text-only [`user_message_frame`] (no attachments).
pub fn user_text_frame(
    session_id: &str,
    user_id: &str,
    text: &str,
    platform_msg_id: &str,
) -> Frame {
    user_message_frame(session_id, user_id, text, platform_msg_id, Vec::new())
}

/// The `user_id` the gateway stamps on web-channel (`http`) messages. The direct
/// (non-relay) transport registers as a web client, so its outbound user
/// messages carry this id (the relay transport uses the device id instead).
pub const WEB_OPERATOR_USER_ID: &str = "web-operator";

/// The [`Frame::Register`] a direct (web-identity) client sends right after the
/// WS upgrade. The gateway ignores the in-frame `token` for web clients (auth is
/// the upgrade-time channel token) and only requires `channel_type == "http"`.
pub fn register_http_frame() -> Frame {
    Frame::Register {
        token: String::new(),
        channel_type: ChannelType::http(),
    }
}

/// Like [`user_message_frame`] but for the direct (web-identity) transport:
/// `channel_type = http` and `user_id = WEB_OPERATOR_USER_ID`. Used over the
/// raw-MessagePack `/v1/channel-ws` leg (no Noise).
pub fn web_user_message_frame(
    session_id: &str,
    text: &str,
    platform_msg_id: &str,
    attachments: Vec<WireAttachment>,
) -> Frame {
    Frame::Message(Message {
        content: text.to_owned(),
        session_id: SessionId::from(session_id),
        user_id: WEB_OPERATOR_USER_ID.to_owned(),
        channel_type: ChannelType::http(),
        bot_id: String::new(),
        attachments,
        platform_msg_id: platform_msg_id.to_owned(),
        role: MessageRole::User,
        ordinal: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// App-initiator + gateway-responder IK handshake; returns the app's session
    /// and the gateway's raw transport (the test drives the gateway by hand).
    fn established_pair() -> (ContentSession, TransportState) {
        let app = StaticKeypair::generate().unwrap();
        let gw = StaticKeypair::generate().unwrap();
        let (handshake, msg1) = ContentHandshake::start(&app, &gw.public()).unwrap();
        let mut gw_hs = gw.ik_responder().unwrap();
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        gw_hs.read_message(&msg1, &mut buf).unwrap();
        let n = gw_hs.write_message(&[], &mut buf).unwrap();
        let session = handshake.finish(&buf[..n]).unwrap();
        let gw_t = gw_hs.into_transport_mode().unwrap();
        (session, gw_t)
    }

    /// Decrypt + reassemble every message the app sealed, returning the frames.
    fn drain(
        reasm: &mut FrameReassembler,
        gw_t: &mut TransportState,
        msgs: &[Vec<u8>],
    ) -> Vec<Frame> {
        let mut frames = Vec::new();
        for m in msgs {
            for bytes in reasm.read(gw_t, m).unwrap() {
                frames.push(decode(&bytes).unwrap());
            }
        }
        frames
    }

    /// The app's content session round-trips a self-pull against an in-process
    /// gateway (IK responder built from the same `device-proto`): the app
    /// subscribes, the gateway replays a `Frame::Message`, the app decodes it.
    #[test]
    fn self_pull_round_trips_over_noise() {
        let (mut session, mut gw_t) = established_pair();
        let mut gw_reasm = FrameReassembler::new();

        // App → gateway: Subscribe for catch-up.
        let subscribe = Frame::Subscribe {
            session_id: SessionId::from("sess-1"),
            since_ordinal: Some(41),
        };
        let sealed = session.seal(&subscribe).unwrap();
        assert_eq!(drain(&mut gw_reasm, &mut gw_t, &sealed), vec![subscribe]);

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
        let reply_msgs = write_chunked(&mut gw_t, &encode(&reply).unwrap()).unwrap();
        let mut decoded = Vec::new();
        for m in &reply_msgs {
            decoded.extend(session.open(m).unwrap());
        }
        assert_eq!(decoded, vec![reply]);
    }

    /// A `Frame` far larger than the Noise per-message ceiling chunks on send and
    /// reassembles intact — the case the old one-frame-per-message path dropped.
    #[test]
    fn content_frame_over_the_ceiling_chunks_and_reassembles() {
        let (mut session, mut gw_t) = established_pair();
        let mut gw_reasm = FrameReassembler::new();

        let big = Frame::Message(Message {
            content: "x".repeat(200_000),
            session_id: SessionId::from("sess-1"),
            user_id: "u1".into(),
            channel_type: ChannelType::ios(),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
            role: MessageRole::User,
            ordinal: None,
        });
        let sealed = session.seal(&big).unwrap();
        assert!(
            sealed.len() > 1,
            "a 200 KB frame spans several Noise messages"
        );
        assert_eq!(drain(&mut gw_reasm, &mut gw_t, &sealed), vec![big]);
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
        let mut buf = [0u8; NOISE_MAX_MESSAGE];
        assert!(gw_hs.read_message(&msg1, &mut buf).is_err());
    }
}
