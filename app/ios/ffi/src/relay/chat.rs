//! The relay chat leg: dial the gateway over the relay's content-join leg, run the
//! Noise IK handshake, and exchange `Frame`s over the established E2E channel.
//!
//! The generic frame pump + session lifecycle live in [`crate::transport`]; this
//! file is just the relay-specific seams: [`RelaySessions::establish`] (dial +
//! Noise) and [`RelayCodec`] (seal/open). The crypto + frame codec themselves live
//! in the local protocol core ([`ContentHandshake`] / [`ContentSession`]).
//!
//! One content leg can subscribe to multiple sessions. Content is relay-only —
//! the app reaches the (possibly NAT'd) gateway through C's blind content-join leg.

use device_proto::noise::StaticKeypair;
use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::Message;

use super::dial::dial_content_join;
use super::pairing::{PairedRecord, load_paired_record};
use crate::core::{ContentHandshake, ContentSession, Frame, user_message_frame};
use crate::transport::{
    ChatTransport, Connection, FrameCodec, SessionLeg, SessionRegistry, TransportError,
    UserFrameFn, WsStream, recv_binary_handshake,
};

/// The relay leg's state: the shared session registry. The durable pairing
/// record is reloaded from the keychain on each connect, so the leg itself is
/// otherwise stateless.
pub(crate) struct RelaySessions {
    registry: SessionRegistry,
}

impl RelaySessions {
    pub(crate) fn new() -> Self {
        Self {
            registry: SessionRegistry::default(),
        }
    }
}

/// The relay frame codec: every `Frame` rides Noise (sealed + chunked on send,
/// decrypted + reassembled on receipt).
struct RelayCodec {
    session: ContentSession,
}

impl FrameCodec for RelayCodec {
    fn encode_outbound(&mut self, frame: &Frame) -> Result<Vec<Vec<u8>>, TransportError> {
        Ok(self.session.seal(frame)?)
    }

    fn decode_inbound(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, TransportError> {
        // A decrypt failure means the Noise stream desynced — unrecoverable, so the
        // pump ends the session on this `Err`.
        Ok(self.session.open(bytes)?)
    }
}

impl ChatTransport for RelaySessions {
    #[allow(clippy::manual_async_fn)]
    fn establish(
        &self,
    ) -> impl std::future::Future<Output = Result<Connection, TransportError>> + Send {
        async move {
            // Preconditions surface as `Precondition` so a transient failure here
            // doesn't tear down a healthy live session on a foreground reconnect
            // (matches the original, which only reset on a failed dial below).
            let record = load_paired_record()
                .map_err(TransportError::Precondition)?
                .ok_or_else(|| {
                    TransportError::Precondition("not paired; pair a gateway first".into())
                })?;
            let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
            if record.relay_node_id.is_empty() {
                return Err(TransportError::Precondition(
                    "paired gateway has no relay route; re-pair".into(),
                ));
            }

            let established = dial_relay(&record, &local).await?;
            let codec: Box<dyn FrameCodec> = Box::new(RelayCodec {
                session: established.session,
            });

            // Relay user messages carry the device id + `channel_type=owner`.
            let device_id = record.device_id.clone();
            let user_frame: UserFrameFn = Box::new(move |session_id, text, msg_id, attachments| {
                user_message_frame(session_id, &device_id, text, msg_id, attachments)
            });

            Ok(Connection {
                ws: established.ws,
                codec,
                user_frame,
            })
        }
    }
}

impl SessionLeg for RelaySessions {
    fn registry(&self) -> &SessionRegistry {
        &self.registry
    }
}

/// An established, handshaken relay leg ready to wrap as a [`Connection`].
struct Established {
    ws: WsStream,
    session: ContentSession,
}

/// Dial the blind relay's content-join leg (shared [`dial_content_join`]) and run
/// the Noise IK handshake over it. The relay admits this leg by the instance key
/// (symmetric with the gateway's host leg); end-to-end, the gateway authenticates
/// this device by matching the Noise IK initiator's static against an approved
/// device row.
async fn dial_relay(
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<Established, TransportError> {
    let ws = dial_content_join(record, None)
        .await
        .map_err(TransportError::Other)?;
    handshake_over(ws, record, local).await
}

/// Run the Noise IK initiator handshake over an established relay WS and return
/// the ready content session.
async fn handshake_over(
    mut ws: WsStream,
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<Established, TransportError> {
    let (handshake, msg1) = ContentHandshake::start(local, &record.gateway_static_pubkey)
        .map_err(|e| TransportError::Other(format!("start handshake: {e}")))?;
    ws.send(Message::Binary(msg1))
        .await
        .map_err(|e| TransportError::Other(format!("send handshake: {e}")))?;
    let msg2 = recv_binary_handshake(&mut ws).await?;
    let session = handshake
        .finish(&msg2)
        .map_err(|e| TransportError::Other(format!("finish handshake: {e}")))?;
    Ok(Established { ws, session })
}
