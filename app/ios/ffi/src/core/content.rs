//! App side of the E2E content session.

use baybo_model::{ChannelType, SessionId};
use device_proto::noise::{FrameReassembler, NOISE_MAX_MESSAGE, StaticKeypair, write_chunked};
use snow::{HandshakeState, TransportState};
use wire::{Frame, Message, MessageRole, WireAttachment, decode, encode};

use super::api_tunnel::ApiTunnelSession;
use super::error::MobileError;

pub struct ContentHandshake {
    state: HandshakeState,
}

impl ContentHandshake {
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

    pub fn finish(mut self, reply: &[u8]) -> Result<ContentSession, MobileError> {
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        self.state.read_message(reply, &mut buf)?;
        Ok(ContentSession {
            transport: self.state.into_transport_mode()?,
            reassembler: FrameReassembler::new(),
        })
    }

    pub fn finish_api_tunnel(mut self, reply: &[u8]) -> Result<ApiTunnelSession, MobileError> {
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        self.state.read_message(reply, &mut buf)?;
        Ok(ApiTunnelSession::new(self.state.into_transport_mode()?))
    }
}

pub struct ContentSession {
    transport: TransportState,
    reassembler: FrameReassembler,
}

impl ContentSession {
    pub fn seal(&mut self, frame: &Frame) -> Result<Vec<Vec<u8>>, MobileError> {
        let plaintext = encode(frame)?;
        Ok(write_chunked(&mut self.transport, &plaintext)?)
    }

    pub fn open(&mut self, message: &[u8]) -> Result<Vec<Frame>, MobileError> {
        let mut frames = Vec::new();
        for bytes in self.reassembler.read(&mut self.transport, message)? {
            frames.push(decode(&bytes)?);
        }
        Ok(frames)
    }
}

pub fn subscribe_frame(session_id: &str, since_ordinal: Option<i64>) -> Frame {
    Frame::Subscribe {
        session_id: SessionId::from(session_id),
        since_ordinal,
    }
}

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
        channel_type: ChannelType::device(),
        bot_id: String::new(),
        attachments,
        platform_msg_id: platform_msg_id.to_owned(),
        role: MessageRole::User,
        ordinal: None,
    })
}

pub fn register_device_frame() -> Frame {
    Frame::Register {
        token: String::new(),
        channel_type: ChannelType::device(),
    }
}
