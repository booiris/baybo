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

use std::sync::Arc;

use remote_host_protocol::relay::LegClass;
use tokio::sync::watch;

use super::carrier::{self, Trigger};
use super::dial::dial_content_join;
use super::leg_pool::BindingKey;
use super::pairing::{PairedRecord, load_paired_record};
use crate::core::{ContentHandshake, ContentSession, Frame, user_message_frame};
use crate::transport::{
    Connection, FrameCodec, LegDialer, LegSocket, RotationGate, SessionLeg, SessionRegistry,
    TransportError, UserFrameFn,
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
            registry: SessionRegistry::new(Arc::new(RelayDialer)),
        }
    }
}

/// The relay leg's dialer — the OWNED establish seam the connection supervisor
/// holds ([`LegDialer`]). Stateless: the pairing record is reloaded from the
/// keychain on every dial.
struct RelayDialer;

impl LegDialer for RelayDialer {
    fn establish(&self) -> futures_util::future::BoxFuture<'_, Result<Connection, TransportError>> {
        Box::pin(async move {
            let (record, local) = load_binding()?;
            let established = dial_relay(&record, &local).await?;
            // The relay chat leg is up: the moment to look for a direct
            // carrier, in the background.
            carrier::hub().request_probe(Trigger::ChatLive);
            Ok(connection(established, &record))
        })
    }

    fn rotation_events(&self) -> Option<watch::Receiver<u64>> {
        Some(carrier::hub().subscribe())
    }

    fn can_rotate(&self) -> bool {
        load_paired_record()
            .ok()
            .flatten()
            .is_some_and(|record| carrier::hub().lease(&BindingKey::from(&record)).is_some())
    }

    /// The chat leg on the direct carrier: `DirectOpen{class: Chat}`, then the
    /// Noise IK handshake and its confirmation. The gateway retires the relay
    /// leg once it reads the confirmation.
    fn rotate(
        &self,
        gate: RotationGate,
    ) -> futures_util::future::BoxFuture<'_, Result<Connection, TransportError>> {
        Box::pin(async move {
            let (record, local) = load_binding()?;
            let hub = carrier::hub();
            let lease = hub
                .lease(&BindingKey::from(&record))
                .ok_or_else(|| TransportError::Other("no direct carrier to rotate onto".into()))?;
            let dialed = carrier::dial_leg(
                &lease.handle,
                LegClass::Chat,
                || gate.commit(),
                |socket| async {
                    handshake_over(socket, &record, &local)
                        .await
                        .map_err(|e| e.to_string())
                },
            )
            .await;
            match dialed {
                Ok(established) => Ok(connection(established, &record)),
                Err(failure) => {
                    hub.dial_failed(lease.id, &failure);
                    Err(TransportError::Other(failure.reason().to_owned()))
                }
            }
        })
    }
}

/// The pairing every chat dial runs for, and the device's Noise static.
/// Preconditions surface as `Precondition` with their own prose ("pair a
/// gateway first"), so the client can tell setup from network.
fn load_binding() -> Result<(PairedRecord, StaticKeypair), TransportError> {
    let record = load_paired_record()
        .map_err(TransportError::Precondition)?
        .ok_or_else(|| TransportError::Precondition("not paired; pair a gateway first".into()))?;
    if record.relay_node_id.is_empty() {
        return Err(TransportError::Precondition(
            "paired gateway has no relay route; re-pair".into(),
        ));
    }
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    Ok((record, local))
}

/// A handshaken chat leg, relay or carrier, as the pump takes it.
fn connection(established: Established, record: &PairedRecord) -> Connection {
    let codec: Box<dyn FrameCodec> = Box::new(RelayCodec {
        session: established.session,
    });
    // Relay user messages carry the device id + `channel_type=owner`.
    let device_id = record.device_id.clone();
    let user_frame: UserFrameFn = Box::new(move |session_id, text, msg_id, attachments| {
        user_message_frame(session_id, &device_id, text, msg_id, attachments)
    });
    Connection {
        socket: established.socket,
        codec,
        user_frame,
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

impl SessionLeg for RelaySessions {
    fn registry(&self) -> &SessionRegistry {
        &self.registry
    }
}

/// An established, handshaken relay leg ready to wrap as a [`Connection`].
struct Established {
    socket: LegSocket,
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
    handshake_over(LegSocket::ws(ws), record, local).await
}

/// Run the Noise IK initiator handshake over a dialed leg socket — confirming
/// it when the socket is a carrier stream — and return the ready content
/// session.
async fn handshake_over(
    mut socket: LegSocket,
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<Established, TransportError> {
    let (handshake, msg1) = ContentHandshake::start(local, &record.gateway_static_pubkey)
        .map_err(|e| TransportError::Other(format!("start handshake: {e}")))?;
    socket
        .send(msg1)
        .await
        .map_err(|e| TransportError::Other(format!("send handshake: {e}")))?;
    let msg2 = socket.recv_handshake().await?;
    let mut session = handshake
        .finish(&msg2)
        .map_err(|e| TransportError::Other(format!("finish handshake: {e}")))?;
    if socket.confirms_handshake() {
        socket
            .send(session.confirmation()?)
            .await
            .map_err(|e| TransportError::Other(format!("send handshake confirmation: {e}")))?;
    }
    Ok(Established { socket, session })
}
