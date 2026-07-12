//! The client half of an API tunnel leg: dial, then drive requests over it.
//!
//! One leg is one ordered Noise stream. [`LegIo`] owns it whole — the socket, the
//! `ApiTunnelSession` (a `TransportState` plus a reassembler), the frames decoded
//! but not yet consumed, and the next request id — because every one of those is
//! only meaningful in lockstep with the others. Handing them around as loose
//! `&mut` arguments is what let the three bugs below live here for as long as they
//! did.
//!
//! ## What a leg may not do
//!
//! * **Interleave requests.** The Noise transport is stateful and ordered;
//!   decrypting out of order is fatal. A `LegIo` is therefore moved, never shared:
//!   whoever holds it is the only writer.
//! * **Survive a desync.** If a response arrives for the wrong request id, or the
//!   body of one request is still on the wire when the next head goes out, the
//!   stream's meaning is gone. There is no recovery — drop the leg.

use std::collections::VecDeque;
use std::time::Duration;

use device_proto::noise::StaticKeypair;
use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::Message;

use super::dial::dial_content_join;
use super::pairing::PairedRecord;
use crate::core::{
    ApiTunnelSession, ContentHandshake, TunnelHeader, TunnelRequest, TunnelResponse,
};
use crate::transport::WsStream;

/// The id every request on a one-shot blob leg carries. Blob legs are never
/// reused (the gateway refuses to advertise reuse on them), so their ids never
/// need to advance.
pub(crate) const BLOB_REQUEST_ID: u64 = 1;

const TUNNEL_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// A leg that can no longer be trusted, and why.
///
/// The distinction is the whole point: a `Dead` leg must never be reused, while a
/// gateway that simply answered "no" left the stream perfectly intact.
#[derive(Debug)]
pub(crate) enum LegError {
    /// The transport failed, or the framing desynced. The leg is finished.
    Dead(String),
    /// The gateway answered with a non-2xx status. Its body has been drained, so
    /// the leg itself is still usable.
    Http { status: u16 },
}

impl LegError {
    pub(crate) fn dead(reason: impl Into<String>) -> Self {
        Self::Dead(reason.into())
    }
}

impl std::fmt::Display for LegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dead(reason) => write!(f, "{reason}"),
            Self::Http { status } => write!(f, "api request failed: HTTP {status}"),
        }
    }
}

impl From<LegError> for String {
    fn from(e: LegError) -> Self {
        e.to_string()
    }
}

/// A response head, once it has been matched to the request that asked for it.
///
/// The wire also carries the gateway's `reuse` offer here; nothing reads it yet
/// (every leg is still one-shot), which is exactly the old behaviour.
#[derive(Debug)]
pub(crate) struct ResponseHead {
    pub(crate) status: u16,
    pub(crate) body_len: Option<u64>,
}

/// The wire under a leg: sealed frames in, decoded frames out.
///
/// A seam, not an abstraction for its own sake. It is what makes the framing
/// rules below testable without a relay, a gateway, or a socket — and `recv`
/// returns a `Vec` on purpose, because ONE wire message can decode to several
/// frames and pretending otherwise is one of the bugs this module exists to fix.
pub(crate) trait TunnelFrames: Send {
    fn send(
        &mut self,
        request: &TunnelRequest,
    ) -> impl std::future::Future<Output = Result<(), LegError>> + Send;

    fn recv(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Vec<TunnelResponse>, LegError>> + Send;

    fn close(&mut self) -> impl std::future::Future<Output = ()> + Send;
}

/// The real wire: msgpack frames sealed into a Noise transport over a WebSocket.
pub(crate) struct NoiseFrames {
    ws: WsStream,
    session: ApiTunnelSession,
}

impl TunnelFrames for NoiseFrames {
    async fn send(&mut self, request: &TunnelRequest) -> Result<(), LegError> {
        for message in self
            .session
            .seal(request)
            .map_err(|e| LegError::dead(format!("seal tunnel request: {e}")))?
        {
            self.ws
                .send(Message::Binary(message))
                .await
                .map_err(|e| LegError::dead(format!("send tunnel request: {e}")))?;
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Vec<TunnelResponse>, LegError> {
        let bytes = crate::transport::recv_binary(&mut self.ws)
            .await
            .map_err(|e| LegError::dead(e.to_string()))?;
        self.session
            .open(&bytes)
            .map_err(|e| LegError::dead(format!("open tunnel response: {e}")))
    }

    async fn close(&mut self) {
        let _ = self.ws.close(None).await;
    }
}

/// A dialed, handshaked API tunnel leg.
pub(crate) struct LegIo<F: TunnelFrames = NoiseFrames> {
    frames: F,
    /// Frames decoded but not yet consumed. The reader used to take the first of a
    /// message's frames and DROP the rest — which held only because the gateway
    /// happens to seal one frame per WebSocket message. That is a coupling, not an
    /// invariant.
    pending: VecDeque<TunnelResponse>,
    /// Ids must strictly increase on a leg; the gateway closes it otherwise.
    next_request_id: u64,
}

impl<F: TunnelFrames> LegIo<F> {
    pub(crate) fn new(frames: F) -> Self {
        Self {
            frames,
            pending: VecDeque::new(),
            next_request_id: 1,
        }
    }

    /// Claim the next request id on this leg.
    pub(crate) fn claim_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    pub(crate) async fn send(&mut self, request: &TunnelRequest) -> Result<(), LegError> {
        self.frames.send(request).await
    }

    /// The next frame on the leg, whatever it is.
    pub(crate) async fn next_response(&mut self) -> Result<TunnelResponse, LegError> {
        loop {
            if let Some(response) = self.pending.pop_front() {
                return Ok(response);
            }
            self.pending.extend(self.frames.recv().await?);
        }
    }

    /// The head of `request_id`'s response.
    ///
    /// A frame tagged with any other id means the leg's framing is not what we
    /// think it is — most likely the tail of a previous request we failed to
    /// drain. Answering the caller with it would hand request N's bytes to
    /// request N+1, so fail fast instead.
    ///
    /// Note there is no loop here, and that is the fix: the old reader SKIPPED
    /// any frame that was not the shape it wanted, which is precisely how a
    /// leftover body would have been swallowed on the way to the next head.
    pub(crate) async fn expect_response_head(
        &mut self,
        request_id: u64,
    ) -> Result<ResponseHead, LegError> {
        match self.next_response().await? {
            TunnelResponse::Head {
                request_id: id,
                status,
                body_len,
                ..
            } => {
                self.check_id(id, request_id)?;
                Ok(ResponseHead { status, body_len })
            }
            // The gateway sends an Error INSTEAD of a head, and only ever when the
            // leg is already finished on its side (an Error frame has no `reuse`
            // field, so it could not say otherwise even if it wanted to).
            TunnelResponse::Error { status, reason, .. } => {
                Err(LegError::dead(format!("HTTP {status}: {reason}")))
            }
            TunnelResponse::Body { request_id: id, .. } => {
                self.check_id(id, request_id)?;
                Err(LegError::dead(
                    "tunnel body arrived before its response head".to_string(),
                ))
            }
        }
    }

    /// The body of `request_id`'s response, whatever its status — a non-2xx has a
    /// body too, and leaving it on the wire poisons whatever comes next.
    pub(crate) async fn collect_response_body(
        &mut self,
        request_id: u64,
        body_len: Option<u64>,
    ) -> Result<Vec<u8>, LegError> {
        let mut body = Vec::new();
        if body_len == Some(0) {
            return Ok(body);
        }
        loop {
            match self.next_response().await? {
                TunnelResponse::Body {
                    request_id: id,
                    offset,
                    data,
                    last,
                } => {
                    self.check_id(id, request_id)?;
                    if offset != body.len() as u64 {
                        return Err(LegError::dead(format!(
                            "body offset {offset} != expected {}",
                            body.len()
                        )));
                    }
                    body.extend(data);
                    if last {
                        return Ok(body);
                    }
                }
                TunnelResponse::Error { status, reason, .. } => {
                    return Err(LegError::dead(format!("HTTP {status}: {reason}")));
                }
                TunnelResponse::Head { request_id: id, .. } => {
                    self.check_id(id, request_id)?;
                    return Err(LegError::dead(
                        "a second response head arrived mid-body".to_string(),
                    ));
                }
            }
        }
    }

    fn check_id(&self, got: u64, expected: u64) -> Result<(), LegError> {
        if got == expected {
            return Ok(());
        }
        Err(LegError::dead(format!(
            "tunnel response for request {got}, expected {expected}"
        )))
    }

    pub(crate) async fn close(mut self) {
        self.frames.close().await;
    }
}

pub(crate) async fn dial_tunnel_leg(
    record: &PairedRecord,
    local: &StaticKeypair,
    leg_class: remote_host_protocol::relay::LegClass,
) -> Result<LegIo, String> {
    let mut ws = dial_content_join(record, Some(leg_class)).await?;
    let (handshake, msg1) = ContentHandshake::start(local, &record.gateway_static_pubkey)
        .map_err(|e| format!("start handshake: {e}"))?;
    ws.send(Message::Binary(msg1))
        .await
        .map_err(|e| format!("send handshake: {e}"))?;
    let msg2 = recv_binary_with_timeout(&mut ws, TUNNEL_HANDSHAKE_TIMEOUT).await?;
    let session = handshake
        .finish_api_tunnel(&msg2)
        .map_err(|e| format!("finish handshake: {e}"))?;
    Ok(LegIo::new(NoiseFrames { ws, session }))
}

async fn recv_binary_with_timeout(ws: &mut WsStream, timeout: Duration) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, crate::transport::recv_binary(ws))
        .await
        .map_err(|_| "handshake timed out".to_string())?
        .map_err(|e| e.to_string())
}

/// Body chunks for `request_id`, sized to the tunnel's chunk ceiling.
///
/// An EMPTY body is no body at all: the head declares `body_len: None` and not a
/// single `Body` frame goes out. The gateway folds a declared length of zero into
/// its no-body branch and NEVER reads that frame — on a one-shot leg the close
/// reclaimed it, but on a reused one it surfaces as the next request's first
/// frame. (The gateway drops such a straggler by id, but that seatbelt exists for
/// clients already in the field; this one must not need it.)
pub(crate) fn body_frames(request_id: u64, body: &[u8]) -> Vec<TunnelRequest> {
    let size = body.len() as u64;
    let mut offset = 0u64;
    body.chunks(crate::core::MAX_TUNNEL_CHUNK)
        .map(|chunk| {
            let chunk_len = chunk.len() as u64;
            let frame = TunnelRequest::Body {
                request_id,
                offset,
                data: chunk.to_vec(),
                last: offset + chunk_len >= size,
            };
            offset += chunk_len;
            frame
        })
        .collect()
}

/// The `Content-Length`-shaped declaration for a request body: `None` for an
/// empty one, so it takes the gateway's no-body path.
pub(crate) fn declared_body_len(body: Option<&[u8]>) -> Option<u64> {
    match body {
        Some(body) if !body.is_empty() => Some(body.len() as u64),
        _ => None,
    }
}

pub(crate) fn content_length_header(len: u64) -> TunnelHeader {
    TunnelHeader::new("content-length", len.to_string())
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::VecDeque as Deque;

    /// A scripted gateway. Each entry is ONE wire message, which may decode to
    /// several frames — the shape that used to lose all but the first.
    pub(crate) struct ScriptedFrames {
        pub(crate) script: Deque<Vec<TunnelResponse>>,
        pub(crate) sent: Vec<TunnelRequest>,
        pub(crate) closed: bool,
    }

    impl ScriptedFrames {
        pub(crate) fn new(script: Vec<Vec<TunnelResponse>>) -> Self {
            Self {
                script: script.into(),
                sent: Vec::new(),
                closed: false,
            }
        }
    }

    impl TunnelFrames for ScriptedFrames {
        async fn send(&mut self, request: &TunnelRequest) -> Result<(), LegError> {
            self.sent.push(request.clone());
            Ok(())
        }

        async fn recv(&mut self) -> Result<Vec<TunnelResponse>, LegError> {
            self.script
                .pop_front()
                .ok_or_else(|| LegError::dead("scripted gateway ran out of messages"))
        }

        async fn close(&mut self) {
            self.closed = true;
        }
    }

    pub(crate) fn head(request_id: u64, status: u16, body_len: u64) -> TunnelResponse {
        TunnelResponse::Head {
            request_id,
            status,
            headers: vec![],
            body_len: Some(body_len),
            reuse: None,
        }
    }

    pub(crate) fn body(request_id: u64, data: &[u8]) -> TunnelResponse {
        TunnelResponse::Body {
            request_id,
            offset: 0,
            data: data.to_vec(),
            last: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    /// One wire message can decode to several frames. The reader took the first and
    /// dropped the rest — safe only for as long as the gateway happened to seal one
    /// frame per message, which is a coupling nobody wrote down.
    #[tokio::test]
    async fn every_frame_in_one_wire_message_is_kept() {
        let mut leg = LegIo::new(ScriptedFrames::new(vec![vec![
            head(1, 200, 2),
            body(1, b"hi"),
        ]]));

        let id = leg.claim_request_id();
        let response = leg.expect_response_head(id).await.expect("head");
        assert_eq!(response.status, 200);
        assert_eq!(
            leg.collect_response_body(id, response.body_len)
                .await
                .expect("body"),
            b"hi",
            "the body rode the SAME message as the head"
        );
    }

    /// A non-2xx carries a body like anything else. Returning without draining it
    /// leaves those bytes on the wire for the next request to mistake for its own —
    /// and `chat_lookup_message`'s 404 is the ordinary result of an outbox rebase,
    /// so this is a hot path, not a corner.
    #[tokio::test]
    async fn a_404_body_is_drained_and_the_next_request_gets_its_own() {
        let mut leg = LegIo::new(ScriptedFrames::new(vec![
            vec![head(1, 404, 9)],
            vec![body(1, b"not found")],
            vec![head(2, 200, 4)],
            vec![body(2, b"[{}]")],
        ]));

        let first = leg.claim_request_id();
        let response = leg.expect_response_head(first).await.expect("head");
        assert_eq!(response.status, 404);
        assert_eq!(
            leg.collect_response_body(first, response.body_len)
                .await
                .expect("the 404's body must be drained"),
            b"not found"
        );

        let second = leg.claim_request_id();
        assert_eq!(second, 2, "ids strictly increase on a leg");
        let response = leg.expect_response_head(second).await.expect("head");
        let body = leg
            .collect_response_body(second, response.body_len)
            .await
            .expect("body");
        assert_eq!(body, b"[{}]", "and not the tail of the 404");
    }

    /// The fail-fast that makes the drain rule enforceable: a frame for the wrong
    /// request is a desync, not something to skip past. The old reader silently
    /// skipped any frame it did not want — which is exactly how request N's
    /// leftovers would have been handed to request N+1.
    #[tokio::test]
    async fn a_response_for_another_request_kills_the_leg() {
        let mut leg = LegIo::new(ScriptedFrames::new(vec![vec![head(7, 200, 0)]]));

        let id = leg.claim_request_id();
        let err = leg
            .expect_response_head(id)
            .await
            .expect_err("a head for request 7 must not answer request 1");
        assert!(matches!(err, LegError::Dead(_)), "{err:?}");
    }

    /// The same rule inside the body: a stray body frame from a previous request
    /// must not be appended to this one's.
    #[tokio::test]
    async fn a_body_for_another_request_kills_the_leg() {
        let mut leg = LegIo::new(ScriptedFrames::new(vec![
            vec![head(1, 200, 4)],
            vec![body(2, b"nope")],
        ]));

        let id = leg.claim_request_id();
        let response = leg.expect_response_head(id).await.expect("head");
        let err = leg
            .collect_response_body(id, response.body_len)
            .await
            .expect_err("a body for request 2 must not fill request 1");
        assert!(matches!(err, LegError::Dead(_)), "{err:?}");
    }

    /// An `Error` frame is the gateway saying the leg is finished — it has no
    /// `reuse` field, so it could not say otherwise even if it wanted to.
    #[tokio::test]
    async fn an_error_frame_kills_the_leg() {
        let mut leg = LegIo::new(ScriptedFrames::new(vec![vec![TunnelResponse::Error {
            request_id: 1,
            status: 400,
            reason: "forbidden tunnel header".into(),
        }]]));

        let id = leg.claim_request_id();
        let err = leg.expect_response_head(id).await.expect_err("an error");
        assert!(matches!(err, LegError::Dead(_)), "{err:?}");
    }

    #[test]
    fn an_empty_body_produces_no_frames_and_no_declared_length() {
        assert!(body_frames(1, &[]).is_empty());
        assert_eq!(declared_body_len(Some(&[])), None);
        assert_eq!(declared_body_len(None), None);
        assert_eq!(declared_body_len(Some(&[1, 2, 3])), Some(3));
    }

    #[test]
    fn a_body_is_chunked_contiguously_with_last_only_at_the_end() {
        let body = vec![7u8; crate::core::MAX_TUNNEL_CHUNK + 1];
        let frames = body_frames(9, &body);

        assert_eq!(frames.len(), 2);
        let mut expected_offset = 0u64;
        for (i, frame) in frames.iter().enumerate() {
            let TunnelRequest::Body {
                request_id,
                offset,
                data,
                last,
            } = frame
            else {
                panic!("expected a body frame, got {frame:?}");
            };
            assert_eq!(*request_id, 9);
            assert_eq!(*offset, expected_offset);
            assert_eq!(*last, i == 1, "only the final chunk is `last`");
            expected_offset += data.len() as u64;
        }
        assert_eq!(expected_offset, body.len() as u64);
    }

    #[test]
    fn a_non_2xx_renders_as_it_always_did() {
        assert_eq!(
            LegError::Http { status: 404 }.to_string(),
            "api request failed: HTTP 404"
        );
        assert_eq!(LegError::dead("socket died").to_string(), "socket died");
    }
}
