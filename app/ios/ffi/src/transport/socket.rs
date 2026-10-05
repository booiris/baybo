//! The binary transport under a leg: a WebSocket (the relay's content-join leg,
//! or the direct login's `/v1/channel-ws`) or one stream of a direct carrier.
//! Either way every message is one Noise message (relay, carrier) or one
//! msgpack frame (direct); a carrier stream frames them with `carrier::framing`.

use carrier::framing::{FrameReader, write_frame};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use super::{HANDSHAKE_REPLY_TIMEOUT, TransportError, WsStream};

/// One bidirectional stream of a direct carrier, after its `DirectOpen` preface.
pub(crate) struct CarrierStream {
    send: quinn::SendStream,
    frames: FrameReader<quinn::RecvStream>,
}

impl CarrierStream {
    pub(crate) fn new(send: quinn::SendStream, frames: FrameReader<quinn::RecvStream>) -> Self {
        Self { send, frames }
    }
}

pub(crate) enum LegSocket {
    Ws(Box<WsStream>),
    Carrier(CarrierStream),
}

/// What one read of a leg yielded. Every variant is proof the socket is alive
/// at this instant, which is what the pump's liveness stamp needs.
pub(crate) enum ReadEvent {
    Message(Vec<u8>),
    /// A WebSocket ping, pong or non-binary message: nothing to decode.
    Control,
    /// The peer ended the socket; the text says how, for the log.
    Ended(String),
    Failed(String),
}

impl LegSocket {
    pub(crate) fn ws(ws: WsStream) -> Self {
        Self::Ws(Box::new(ws))
    }

    /// Whether this leg's Noise handshake ends with P's confirmation: an empty
    /// transport message sent right after msg2. Carrier sessions confirm; the
    /// relay leg keeps the two-message handshake (see the gateway's
    /// `BinarySink::CONFIRMS_HANDSHAKE`).
    pub(crate) fn confirms_handshake(&self) -> bool {
        matches!(self, Self::Carrier(_))
    }

    pub(crate) async fn send(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        match self {
            Self::Ws(ws) => ws
                .send(Message::Binary(bytes))
                .await
                .map_err(|e| format!("ws: {e}")),
            Self::Carrier(stream) => write_frame(&mut stream.send, &bytes)
                .await
                .map_err(|e| format!("carrier stream: {e}")),
        }
    }

    /// The next binary message.
    ///
    /// UNBOUNDED, and it must stay that way: this is not a handshake helper. It
    /// is also `relay::tunnel::NoiseFrames::recv`, i.e. the read under every
    /// REST-over-relay response frame and every blob chunk, whose budgets are
    /// owned by their own callers (`POOLED_LEG_FIRST_BYTE_TIMEOUT`,
    /// `TUNNEL_REQUEST_TIMEOUT`, `TUNNEL_HANDSHAKE_TIMEOUT`) and are far wider
    /// than any handshake's. A blanket timeout here silently caps all of them —
    /// and a 100 MiB upload's post-transfer wait, which is deliberately
    /// uncapped. Wrap the CALL SITE that wants a bound; see
    /// [`Self::recv_handshake`].
    pub(crate) async fn recv(&mut self) -> Result<Vec<u8>, TransportError> {
        match self {
            Self::Ws(ws) => loop {
                match ws.next().await {
                    Some(Ok(Message::Binary(b))) => return Ok(b),
                    Some(Ok(Message::Close(_))) | None => {
                        return Err(TransportError::Other("connection closed".into()));
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(TransportError::Other(format!("ws: {e}"))),
                }
            },
            Self::Carrier(stream) => match stream.frames.next_frame().await {
                Ok(Some(bytes)) => Ok(bytes),
                Ok(None) => Err(TransportError::Other("connection closed".into())),
                Err(e) => Err(TransportError::Other(format!("carrier stream: {e}"))),
            },
        }
    }

    /// [`Self::recv`] for the ONE thing that deserves a short leash: a leg's
    /// handshake reply (Noise message 2, the direct leg's `RegisterAck`).
    ///
    /// The upgrade completing does not mean anyone is on the other end yet. The
    /// relay parks a content-join leg the moment it accepts it, and only then
    /// does the gateway dial in to claim it — so a gateway that fails to claim
    /// leaves the phone blocked on a perfectly healthy socket with nothing ever
    /// coming back. Left unbounded that costs the whole connect budget; bounded,
    /// it costs one step of the retry ladder.
    pub(crate) async fn recv_handshake(&mut self) -> Result<Vec<u8>, TransportError> {
        tokio::time::timeout(HANDSHAKE_REPLY_TIMEOUT, self.recv())
            .await
            .map_err(|_| {
                TransportError::Other(format!(
                    "no handshake reply within {}s (the peer accepted the socket but never answered)",
                    HANDSHAKE_REPLY_TIMEOUT.as_secs()
                ))
            })?
    }

    pub(crate) async fn close(&mut self) {
        match self {
            Self::Ws(ws) => {
                let _ = ws.as_mut().close(None).await;
            }
            Self::Carrier(stream) => {
                let _ = stream.send.finish();
            }
        }
    }

    pub(crate) fn split(self) -> (LegWriter, LegReader) {
        match self {
            Self::Ws(ws) => {
                let (sink, stream) = (*ws).split();
                (LegWriter::Ws(sink), LegReader::Ws(stream))
            }
            Self::Carrier(CarrierStream { send, frames }) => {
                (LegWriter::Carrier(send), LegReader::Carrier(frames))
            }
        }
    }
}

pub(crate) enum LegWriter {
    Ws(SplitSink<WsStream, Message>),
    Carrier(quinn::SendStream),
}

impl LegWriter {
    /// Not cancel safe on a carrier stream (a cancelled write may leave part of
    /// a frame behind), so it never sits in a `select!` arm's future.
    pub(crate) async fn send(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        match self {
            Self::Ws(sink) => sink
                .send(Message::Binary(bytes))
                .await
                .map_err(|e| e.to_string()),
            Self::Carrier(send) => write_frame(send, &bytes).await.map_err(|e| e.to_string()),
        }
    }

    pub(crate) async fn close(&mut self) {
        match self {
            Self::Ws(sink) => {
                let _ = sink.close().await;
            }
            Self::Carrier(send) => {
                let _ = send.finish();
            }
        }
    }
}

pub(crate) enum LegReader {
    Ws(SplitStream<WsStream>),
    Carrier(FrameReader<quinn::RecvStream>),
}

impl LegReader {
    /// Cancel safe, so the pump reads inside its `select!`.
    pub(crate) async fn next(&mut self) -> ReadEvent {
        match self {
            Self::Ws(stream) => match stream.next().await {
                Some(Ok(Message::Binary(bytes))) => ReadEvent::Message(bytes),
                Some(Ok(Message::Close(Some(frame)))) => ReadEvent::Ended(format!(
                    "socket closed by peer (code={} reason={:?})",
                    u16::from(frame.code),
                    frame.reason
                )),
                Some(Ok(Message::Close(None))) => {
                    ReadEvent::Ended("socket closed by peer (no close frame body)".into())
                }
                Some(Ok(_)) => ReadEvent::Control,
                None => ReadEvent::Ended("socket stream ended".into()),
                Some(Err(e)) => ReadEvent::Failed(e.to_string()),
            },
            Self::Carrier(frames) => match frames.next_frame().await {
                Ok(Some(bytes)) => ReadEvent::Message(bytes),
                Ok(None) => ReadEvent::Ended("carrier stream ended".into()),
                Err(e) => ReadEvent::Failed(e.to_string()),
            },
        }
    }
}
