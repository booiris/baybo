//! Gateway responder for E2E API tunnel relay legs.
//!
//! `LegClass::Api` and `LegClass::Blob` both enter here. The relay meters `Api`
//! as interactive and `Blob` as background, but after the shared Noise IK device
//! authentication the plaintext is the same HTTP-shaped tunnel protocol.
//!
//! An `Api` leg may serve SEVERAL requests in sequence: dialing one costs the
//! phone a WSS upgrade plus a Noise IK handshake (~4 round trips), which the
//! relay-mode client used to pay on every single REST call. The gateway
//! advertises that with [`TunnelReuse`] on each response head — the only
//! capability signal, and one an old client simply ignores.
//!
//! A `Blob` leg keeps the original single-request shape. Its transfers run to
//! 100 MiB / ten minutes, so a leg-lifetime bound would guillotine a legitimate
//! upload, and the relay meters it as background bandwidth. It answers
//! `reuse: None`, which makes "blobs are never pooled" a property of THIS side
//! rather than a promise the client has to keep.
//!
//! ## The framing rule
//!
//! A leg may serve another request only if the previous request's declared body
//! was fully drained (or there was none) AND its response was fully sent.
//! Anything else leaves the Noise stream ambiguous — the peer may still be
//! writing body chunks we will never read — so the leg closes. [`LegState`] and
//! [`BodyDrain`] carry that decision out of every exit path; a one-shot leg made
//! it structurally impossible, and reuse buys it back at this price.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use axum::body::{self, Body};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Uri, header};
use bytes::Bytes;
use device_proto::api_tunnel::{
    self, MAX_TUNNEL_CHUNK, TunnelHeader, TunnelRequest, TunnelResponse, TunnelReuse,
};
use device_proto::noise::{FrameReassembler, write_chunked};
use futures::StreamExt;
use remote_host_protocol::relay::LegClass;
use snow::TransportState;
use tokio_stream::wrappers::ReceiverStream;
use tower::ServiceExt;

use super::blobs::MAX_BLOB_BYTES;
use super::device_content::{
    AuthenticatedDevice, BinarySink, BinarySource, RelaySessionError, RelayWs, TungBinSink,
    TungBinSource, log_relay_session_end, responder_handshake,
};
use super::state::WsChannelState;
use crate::auth::DEVICE_ID_HEADER;

/// How long to wait for the FIRST request head after the handshake. A leg that
/// dials and then says nothing is a leak either way, so this is unchanged — and
/// because it applies on both the old and the new gateway, a client can park a
/// freshly dialed leg for a bounded window with no negotiation at all.
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a SUBSEQUENT request head on an `Api` leg, and the value
/// advertised as `reuse.idle_ms`. Covers the human-paced gaps that dominate the
/// relay REST surface — list appears → user taps in (1-10s), chat opens → first
/// send — without parking legs on the relay for long.
///
/// The relay is a third, independently versioned component with no reaper for
/// data legs (only its control link has an idle timeout), so THIS timeout is the
/// only thing that reclaims them. A parked leg holds one of the relay's
/// non-reserved connection slots, shared with chat legs; that is why this is 60s
/// and not several minutes.
const TUNNEL_REUSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether the leg's framing is still unambiguous enough to serve another
/// request. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegState {
    Reusable,
    MustClose,
}

/// Whether the peer's declared request body was fully consumed.
///
/// `Abandoned` is the one that matters: the router can hang up on the body (a
/// 401 returned before any extractor reads it), leaving the peer still sending
/// chunks we will never read. The response is still delivered — the client needs
/// that 401 — but the leg cannot survive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyDrain {
    Drained,
    Abandoned,
}

/// The reuse offer that may ride out on a response head.
///
/// `reuse` is the ONLY thing the client reads, and `LegState` is the only thing
/// that closes the leg. Computing them independently let them drift: a response
/// on a leg we were about to hang up on still advertised a 60s budget, so the
/// client parked a corpse and served the next call from it. Deriving one from the
/// other makes that unrepresentable.
fn offer(reuse: Option<TunnelReuse>, state: LegState) -> Option<TunnelReuse> {
    match state {
        LegState::Reusable => reuse,
        LegState::MustClose => None,
    }
}
const TUNNEL_UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const TUNNEL_UPLOAD_CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const HEADER_CONTENT_LENGTH: &str = "content-length";
const TUNNEL_HTTP_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
const FORBIDDEN_HEADERS: &[&str] = &[
    "authorization",
    "connection",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    DEVICE_ID_HEADER,
];

struct RequestHead {
    request_id: u64,
    method: String,
    path: String,
    headers: Vec<TunnelHeader>,
    body_len: Option<u64>,
}

pub(crate) async fn run_api_tunnel_over_relay(
    ws: RelayWs,
    class: LegClass,
    state: &WsChannelState,
) {
    let (sink, source) = ws.split();
    if let Err(e) = run_tunnel_session(TungBinSink(sink), TungBinSource(source), class, state).await
    {
        log_relay_session_end(&e, "relay api tunnel aborted");
    }
}

/// The reuse budget this leg advertises, or `None` for a one-shot leg.
///
/// Only `Api` legs are reusable. A `Blob` leg answers `None`, so a client that
/// tried to pool one would simply not — the exclusion is enforced here, not
/// trusted to the phone.
fn reuse_for(class: LegClass) -> Option<TunnelReuse> {
    match class {
        LegClass::Api => Some(TunnelReuse {
            idle_ms: TUNNEL_REUSE_IDLE_TIMEOUT.as_millis() as u32,
        }),
        LegClass::Blob | LegClass::Chat => None,
    }
}

async fn run_tunnel_session<Si: BinarySink, So: BinarySource>(
    mut sink: Si,
    mut source: So,
    class: LegClass,
    state: &WsChannelState,
) -> Result<(), RelaySessionError> {
    let (mut transport, device) = responder_handshake(&mut sink, &mut source, state).await?;
    let mut reassembler = FrameReassembler::new();
    let mut pending = VecDeque::new();
    let reuse = reuse_for(class);
    // Requests served on this leg, and the highest id seen. `served` picks the
    // idle budget and tells EOF-is-fine from EOF-is-an-error; `last_id` is what
    // makes a late frame from an already-answered request distinguishable from a
    // desync.
    let mut served: u64 = 0;
    let mut last_id: u64 = 0;

    tracing::debug!(
        device = %super::short_hash(&device.device_id),
        class = ?class,
        reusable = reuse.is_some(),
        "device api tunnel established",
    );

    loop {
        let idle = if served == 0 {
            TUNNEL_IDLE_TIMEOUT
        } else {
            TUNNEL_REUSE_IDLE_TIMEOUT
        };
        let next = match tokio::time::timeout(
            idle,
            next_request(&mut source, &mut transport, &mut reassembler, &mut pending),
        )
        .await
        {
            // A reused leg idling out is how it is SUPPOSED to end.
            Err(_) if served > 0 => return Ok(()),
            Err(_) => {
                return Err(RelaySessionError::Ended(
                    "timed out waiting for tunnel request".to_string(),
                ));
            }
            Ok(Err(reason)) => return Err(reason.into()),
            // A one-shot client (or an old one) closes after its request. Leave at
            // once rather than sitting on the idle budget.
            Ok(Ok(None)) if served > 0 => return Ok(()),
            Ok(Ok(None)) => {
                return Err(RelaySessionError::Ended(
                    "peer closed before request head".to_string(),
                ));
            }
            Ok(Ok(Some(req))) => req,
        };

        let head = match next {
            // Ids must strictly increase. A repeat is either a replay or a client
            // whose framing we no longer understand; either way this leg is done.
            TunnelRequest::Head { request_id, .. } if request_id <= last_id => {
                send_error(
                    &mut sink,
                    &mut transport,
                    request_id,
                    400,
                    "request id must increase",
                )
                .await?;
                return Ok(());
            }
            TunnelRequest::Head {
                request_id,
                method,
                path,
                headers,
                body_len,
            } => {
                last_id = request_id;
                RequestHead {
                    request_id,
                    method,
                    path,
                    headers,
                    body_len,
                }
            }
            // A late frame for a request already answered. Its only sources are a
            // client that encodes an empty body as `Head{body_len: Some(0)}` plus a
            // `Body{last: true}` the no-body branch below never reads (every shipped
            // client does), and a Cancel that lost its race. Framing is still clean
            // here, because every path that ABANDONS a body drain has already closed
            // the leg.
            TunnelRequest::Body { request_id, .. } | TunnelRequest::Cancel { request_id, .. }
                if request_id <= last_id =>
            {
                continue;
            }
            // A body for an id we have never seen: we cannot know how much of it is
            // coming, so the stream is unreadable from here.
            TunnelRequest::Body { request_id, .. } => {
                send_error(
                    &mut sink,
                    &mut transport,
                    request_id,
                    400,
                    "body sent before request head",
                )
                .await?;
                return Ok(());
            }
            TunnelRequest::Cancel { reason, .. } => {
                return Err(RelaySessionError::Ended(format!(
                    "request canceled: {reason}"
                )));
            }
        };

        if let Err((status, reason)) = validate_request_head(&head) {
            send_error(&mut sink, &mut transport, head.request_id, status, reason).await?;
            return Ok(());
        }

        match handle_http_forward(
            &mut sink,
            &mut source,
            &mut transport,
            &mut reassembler,
            &mut pending,
            state,
            &device,
            head,
            reuse,
        )
        .await?
        {
            LegState::Reusable if reuse.is_some() => served += 1,
            // A one-shot leg: the request is answered, and we never told the client
            // it could send another.
            LegState::Reusable | LegState::MustClose => return Ok(()),
        }
    }
}

async fn send_error<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    request_id: u64,
    status: u16,
    reason: &str,
) -> Result<(), String> {
    send_response(
        sink,
        transport,
        &TunnelResponse::Error {
            request_id,
            status,
            reason: reason.to_owned(),
        },
    )
    .await
}

async fn send_response<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    msg: &TunnelResponse,
) -> Result<(), String> {
    let plaintext = api_tunnel::encode(msg).map_err(|e| format!("encode tunnel response: {e}"))?;
    let messages =
        write_chunked(transport, &plaintext).map_err(|e| format!("seal tunnel response: {e}"))?;
    for message in messages {
        sink.send_bytes(message)
            .await
            .map_err(|()| "send tunnel response".to_string())?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_http_forward<Si: BinarySink, So: BinarySource>(
    sink: &mut Si,
    source: &mut So,
    transport: &mut TransportState,
    reassembler: &mut FrameReassembler,
    pending: &mut VecDeque<TunnelRequest>,
    state: &WsChannelState,
    device: &AuthenticatedDevice,
    head: RequestHead,
    reuse: Option<TunnelReuse>,
) -> Result<LegState, String> {
    if request_body_len(&head) != 0 {
        return handle_http_body_forward(
            sink,
            source,
            transport,
            reassembler,
            pending,
            state,
            device,
            head,
            reuse,
        )
        .await;
    }

    let req = match build_forward_request(
        &head,
        device,
        state.tunnel_http.auth.admin_bearer(),
        Body::empty(),
    ) {
        Ok(req) => req,
        Err((status, reason)) => {
            send_error(sink, transport, head.request_id, status, &reason).await?;
            return Ok(LegState::MustClose);
        }
    };
    let response = super::tunnel_http::router(state.clone())
        .oneshot(req)
        .await
        .map_err(|e| format!("forward tunnel HTTP request: {e}"))?;
    // Nothing to drain and the response goes out whole, so this leg survives.
    let leg_state = LegState::Reusable;
    send_http_response(
        sink,
        transport,
        head.request_id,
        response,
        offer(reuse, leg_state),
    )
    .await?;
    Ok(leg_state)
}

#[allow(clippy::too_many_arguments)]
async fn handle_http_body_forward<Si: BinarySink, So: BinarySource>(
    sink: &mut Si,
    source: &mut So,
    transport: &mut TransportState,
    reassembler: &mut FrameReassembler,
    pending: &mut VecDeque<TunnelRequest>,
    state: &WsChannelState,
    device: &AuthenticatedDevice,
    head: RequestHead,
    reuse: Option<TunnelReuse>,
) -> Result<LegState, String> {
    let declared_len = request_body_len(&head);
    // The peer is (or is about to be) streaming a body we are refusing. We never
    // read it, so the stream is left mid-message: answer, then close.
    if declared_len > MAX_BLOB_BYTES as u64 {
        send_error(
            sink,
            transport,
            head.request_id,
            413,
            "request body exceeds size limit",
        )
        .await?;
        return Ok(LegState::MustClose);
    }

    let request_id = head.request_id;
    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(4);
    let req = match build_forward_request(
        &head,
        device,
        state.tunnel_http.auth.admin_bearer(),
        Body::from_stream(ReceiverStream::new(rx)),
    ) {
        Ok(req) => req,
        Err((status, reason)) => {
            close_forward_body(tx, reason.clone()).await;
            send_error(sink, transport, request_id, status, &reason).await?;
            return Ok(LegState::MustClose);
        }
    };

    let router = super::tunnel_http::router(state.clone());
    let response_task = tokio::spawn(async move {
        router
            .oneshot(req)
            .await
            .map_err(|e| format!("forward tunnel HTTP request: {e}"))
    });
    let drain = match stream_forward_body(
        source,
        transport,
        reassembler,
        pending,
        tx,
        request_id,
        declared_len,
    )
    .await
    {
        Ok(drain) => drain,
        Err((status, reason)) => {
            response_task.abort();
            let _ = response_task.await;
            send_error(sink, transport, request_id, status, &reason).await?;
            return Ok(LegState::MustClose);
        }
    };

    let response = response_task
        .await
        .map_err(|e| format!("forward tunnel HTTP task failed: {e}"))??;
    let leg_state = match drain {
        BodyDrain::Drained => LegState::Reusable,
        // The router answered without reading the body (a 401 before any extractor
        // touched it). The peer may still be writing chunks addressed to a request
        // we have finished, so the next `next_request` would decode them as this
        // leg's future. Deliver the response — the client needs that status — then
        // hang up, and say so on the head: `offer` is what stops us telling the
        // client to keep a leg we are closing under it.
        BodyDrain::Abandoned => LegState::MustClose,
    };
    send_http_response(
        sink,
        transport,
        request_id,
        response,
        offer(reuse, leg_state),
    )
    .await?;
    Ok(leg_state)
}

/// Synthesise the in-process HTTP request for an already-Noise-authenticated
/// device. `admin_bearer` is the gateway's own admin token — see
/// [`crate::auth::AdminAuthState::admin_bearer`] for why it, and not the
/// device's bearer, is what this presents.
fn build_forward_request(
    head: &RequestHead,
    device: &AuthenticatedDevice,
    admin_bearer: &str,
    body: Body,
) -> Result<Request<Body>, (u16, String)> {
    let method = head
        .method
        .parse::<Method>()
        .map_err(|e| (400, format!("invalid request method: {e}")))?;
    let uri = head
        .path
        .parse::<Uri>()
        .map_err(|e| (400, format!("invalid request path: {e}")))?;
    let mut builder = Request::builder().method(method).uri(uri);
    let header_content_len = tunnel_header_content_length(&head.headers)?;
    let effective_body_len = match (header_content_len, head.body_len) {
        (Some(header_len), Some(body_len)) if header_len != body_len => {
            return Err((
                400,
                "content-length does not match tunnel body length".to_string(),
            ));
        }
        (Some(header_len), _) => Some(header_len),
        (None, body_len) => body_len,
    };
    for header in &head.headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|e| (400, format!("invalid header name: {e}")))?;
        if name.as_str().eq_ignore_ascii_case(HEADER_CONTENT_LENGTH) {
            continue;
        }
        let value = HeaderValue::from_str(&header.value)
            .map_err(|e| (400, format!("invalid header value: {e}")))?;
        builder = builder.header(name, value);
    }
    if let Some(body_len) = effective_body_len {
        builder = builder.header(HEADER_CONTENT_LENGTH, body_len.to_string());
    }
    builder = builder
        .header(header::AUTHORIZATION, format!("Bearer {admin_bearer}"))
        .header(DEVICE_ID_HEADER, device.device_id.as_str());
    let req = builder
        .body(body)
        .map_err(|e| (400, format!("invalid forwarded request: {e}")))?;
    Ok(req)
}

async fn stream_forward_body<So: BinarySource>(
    source: &mut So,
    transport: &mut TransportState,
    reassembler: &mut FrameReassembler,
    pending: &mut VecDeque<TunnelRequest>,
    tx: tokio::sync::mpsc::Sender<io::Result<Bytes>>,
    request_id: u64,
    declared_len: u64,
) -> Result<BodyDrain, (u16, String)> {
    if declared_len == 0 {
        drop(tx);
        return Ok(BodyDrain::Drained);
    }

    let mut received = 0u64;
    let upload_deadline = tokio::time::Instant::now() + TUNNEL_UPLOAD_TOTAL_TIMEOUT;
    loop {
        let remaining = upload_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            close_forward_body(tx, "upload timed out").await;
            return Err((408, "upload timed out".to_string()));
        }

        let req = match tokio::time::timeout(
            std::cmp::min(remaining, TUNNEL_UPLOAD_CHUNK_IDLE_TIMEOUT),
            next_request(source, transport, reassembler, pending),
        )
        .await
        {
            Ok(Ok(Some(req))) => req,
            Ok(Ok(None)) => {
                close_forward_body(tx, "upload leg closed").await;
                return Err((400, "upload leg closed".to_string()));
            }
            Ok(Err(reason)) => {
                close_forward_body(tx, reason.clone()).await;
                return Err((400, reason));
            }
            Err(_) => {
                close_forward_body(tx, "upload timed out").await;
                return Err((408, "upload timed out".to_string()));
            }
        };

        match req {
            TunnelRequest::Body {
                request_id: body_request_id,
                offset,
                data,
                last,
            } if body_request_id == request_id => {
                if data.len() > MAX_TUNNEL_CHUNK || offset != received {
                    close_forward_body(tx, "upload chunk offset mismatch").await;
                    return Err((400, "chunk offset mismatch".to_string()));
                }
                let next_received = received.saturating_add(data.len() as u64);
                if next_received > declared_len {
                    close_forward_body(tx, "upload exceeded content length").await;
                    return Err((400, "upload exceeded content length".to_string()));
                }
                if last && next_received != declared_len {
                    close_forward_body(tx, "upload ended before content length").await;
                    return Err((400, "upload ended before content length".to_string()));
                }
                // The router dropped the body receiver — it answered without ever
                // reading the request body (a 401 rejected before any extractor ran
                // is the common one). We stop reading, but the peer does not stop
                // writing: the rest of its chunks are still coming.
                if tx.send(Ok(Bytes::from(data))).await.is_err() {
                    return Ok(BodyDrain::Abandoned);
                }
                received = next_received;
                if last {
                    break;
                }
            }
            TunnelRequest::Cancel {
                request_id: cancel_request_id,
                reason,
            } if cancel_request_id == request_id => {
                close_forward_body(tx, "upload canceled").await;
                return Err((499, reason));
            }
            _ => {
                close_forward_body(tx, "unexpected upload message").await;
                return Err((400, "unexpected message during upload".to_string()));
            }
        }
    }

    drop(tx);
    Ok(BodyDrain::Drained)
}

async fn close_forward_body(
    tx: tokio::sync::mpsc::Sender<io::Result<Bytes>>,
    reason: impl Into<String>,
) {
    let _ = tx.send(Err(io::Error::other(reason.into()))).await;
    drop(tx);
}

async fn send_http_response<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    request_id: u64,
    response: axum::response::Response,
    reuse: Option<TunnelReuse>,
) -> Result<(), String> {
    let (parts, body) = response.into_parts();
    if let Some(body_len) = response_content_length(&parts.headers) {
        send_response(
            sink,
            transport,
            &TunnelResponse::Head {
                request_id,
                status: parts.status.as_u16(),
                headers: forwarded_response_headers(&parts.headers, Some(body_len)),
                body_len: Some(body_len),
                reuse,
            },
        )
        .await?;
        return send_streaming_http_body(sink, transport, request_id, body, body_len).await;
    }

    if parts
        .headers
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| {
            value
                .to_str()
                .is_ok_and(|s| s.starts_with("text/event-stream"))
        })
    {
        send_response(
            sink,
            transport,
            &TunnelResponse::Head {
                request_id,
                status: parts.status.as_u16(),
                headers: forwarded_response_headers(&parts.headers, None),
                body_len: None,
                reuse,
            },
        )
        .await?;
        return send_streaming_http_body_without_len(sink, transport, request_id, body).await;
    }

    let body = body::to_bytes(body, TUNNEL_HTTP_RESPONSE_LIMIT)
        .await
        .map_err(|e| format!("read forwarded response body: {e}"))?;
    let body_len = body.len() as u64;
    send_response(
        sink,
        transport,
        &TunnelResponse::Head {
            request_id,
            status: parts.status.as_u16(),
            headers: forwarded_response_headers(&parts.headers, Some(body_len)),
            body_len: Some(body_len),
            reuse,
        },
    )
    .await?;
    if body.is_empty() {
        return Ok(());
    }
    let mut offset = 0usize;
    while offset < body.len() {
        let end = std::cmp::min(offset + MAX_TUNNEL_CHUNK, body.len());
        send_response(
            sink,
            transport,
            &TunnelResponse::Body {
                request_id,
                offset: offset as u64,
                data: body[offset..end].to_vec(),
                last: end == body.len(),
            },
        )
        .await?;
        offset = end;
    }
    Ok(())
}

async fn send_streaming_http_body<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    request_id: u64,
    body: Body,
    body_len: u64,
) -> Result<(), String> {
    if body_len == 0 {
        return Ok(());
    }

    let mut offset = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read forwarded response body: {e}"))?;
        let mut chunk_offset = 0usize;
        while chunk_offset < chunk.len() {
            let end = std::cmp::min(chunk_offset + MAX_TUNNEL_CHUNK, chunk.len());
            let next_offset = offset.saturating_add((end - chunk_offset) as u64);
            if next_offset > body_len {
                return Err("forwarded response exceeded content length".to_string());
            }
            send_response(
                sink,
                transport,
                &TunnelResponse::Body {
                    request_id,
                    offset,
                    data: chunk[chunk_offset..end].to_vec(),
                    last: next_offset >= body_len,
                },
            )
            .await?;
            offset = next_offset;
            chunk_offset = end;
        }
    }

    if offset != body_len {
        return Err(format!(
            "forwarded response ended at {offset} of {body_len} bytes"
        ));
    }
    Ok(())
}

async fn send_streaming_http_body_without_len<S: BinarySink>(
    sink: &mut S,
    transport: &mut TransportState,
    request_id: u64,
    body: Body,
) -> Result<(), String> {
    let mut offset = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read forwarded response body: {e}"))?;
        let mut chunk_offset = 0usize;
        while chunk_offset < chunk.len() {
            let end = std::cmp::min(chunk_offset + MAX_TUNNEL_CHUNK, chunk.len());
            let next_offset = offset.saturating_add((end - chunk_offset) as u64);
            send_response(
                sink,
                transport,
                &TunnelResponse::Body {
                    request_id,
                    offset,
                    data: chunk[chunk_offset..end].to_vec(),
                    last: false,
                },
            )
            .await?;
            offset = next_offset;
            chunk_offset = end;
        }
    }
    send_response(
        sink,
        transport,
        &TunnelResponse::Body {
            request_id,
            offset,
            data: Vec::new(),
            last: true,
        },
    )
    .await
}

fn forwarded_response_headers(headers: &HeaderMap, body_len: Option<u64>) -> Vec<TunnelHeader> {
    let mut out = Vec::new();
    let mut has_content_length = false;
    for (name, value) in headers {
        let name = name.as_str();
        if FORBIDDEN_HEADERS.contains(&name) {
            continue;
        }
        if name.eq_ignore_ascii_case(HEADER_CONTENT_LENGTH) {
            has_content_length = true;
        }
        if let Ok(value) = value.to_str() {
            out.push(TunnelHeader::new(name, value));
        }
    }
    if !has_content_length && let Some(body_len) = body_len {
        out.push(TunnelHeader::new(
            HEADER_CONTENT_LENGTH,
            body_len.to_string(),
        ));
    }
    out
}

async fn next_request<So: BinarySource>(
    source: &mut So,
    transport: &mut TransportState,
    reassembler: &mut FrameReassembler,
    pending: &mut VecDeque<TunnelRequest>,
) -> Result<Option<TunnelRequest>, String> {
    if let Some(req) = pending.pop_front() {
        return Ok(Some(req));
    }
    loop {
        let Some(bytes) = source.next_bytes().await else {
            return Ok(None);
        };
        let frames = reassembler
            .read(transport, &bytes)
            .map_err(|e| format!("open tunnel request: {e}"))?;
        for frame in frames {
            let req = api_tunnel::decode::<TunnelRequest>(&frame)
                .map_err(|e| format!("decode tunnel request: {e}"))?;
            pending.push_back(req);
        }
        if let Some(req) = pending.pop_front() {
            return Ok(Some(req));
        }
    }
}

fn request_body_len(head: &RequestHead) -> u64 {
    head.body_len
        .or_else(|| tunnel_header_content_length(&head.headers).ok().flatten())
        .unwrap_or(0)
}

fn tunnel_header_content_length(headers: &[TunnelHeader]) -> Result<Option<u64>, (u16, String)> {
    let mut out = None;
    for header in headers {
        if !header.name.eq_ignore_ascii_case(HEADER_CONTENT_LENGTH) {
            continue;
        }
        let len = header
            .value
            .parse::<u64>()
            .map_err(|e| (400, format!("invalid content-length header: {e}")))?;
        if let Some(existing) = out
            && existing != len
        {
            return Err((400, "conflicting content-length headers".to_string()));
        }
        out = Some(len);
    }
    Ok(out)
}

fn response_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(HEADER_CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn validate_request_head(head: &RequestHead) -> Result<(), (u16, &'static str)> {
    if head.method.trim().is_empty() {
        return Err((400, "missing request method"));
    }
    if !head.path.starts_with('/') || head.path.starts_with("//") || head.path.contains("://") {
        return Err((400, "absolute URLs are not allowed"));
    }
    if !head.path.starts_with("/v1/") && head.path != "/v1" {
        return Err((403, "path is not allowed"));
    }
    let path_only = head.path.split('?').next().unwrap_or(head.path.as_str());
    for segment in path_only.split('/') {
        let lower = segment.to_ascii_lowercase();
        if segment == ".." || lower == "%2e%2e" || lower.contains("%2e%2e") {
            return Err((400, "path traversal is not allowed"));
        }
    }
    for header in &head.headers {
        let name = header.name.trim().to_ascii_lowercase();
        if name.is_empty() || name.contains('\r') || name.contains('\n') {
            return Err((400, "invalid header name"));
        }
        if FORBIDDEN_HEADERS.contains(&name.as_str()) {
            return Err((400, "forbidden tunnel header"));
        }
        if header.value.contains('\r') || header.value.contains('\n') {
            return Err((400, "invalid header value"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(headers: Vec<TunnelHeader>) -> RequestHead {
        RequestHead {
            request_id: 1,
            method: "GET".into(),
            path: "/v1/chat/sessions".into(),
            headers,
            body_len: None,
        }
    }

    #[test]
    fn forward_request_injects_gateway_device_auth_headers() {
        let device = AuthenticatedDevice {
            device_id: "device-abc".into(),
        };
        let req = build_forward_request(
            &head(vec![TunnelHeader::new("accept", "application/json")]),
            &device,
            "admin-bearer",
            Body::empty(),
        )
        .expect("request");

        // The gateway's own admin bearer, not the device's — storage keeps only
        // a digest of the latter, and the middleware's admin-token + device-id
        // branch is what preserves the Device identity downstream.
        assert_eq!(
            req.headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer admin-bearer"),
        );
        assert_eq!(
            req.headers()
                .get(DEVICE_ID_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some("device-abc"),
        );
        assert_eq!(
            req.headers().get("accept").and_then(|v| v.to_str().ok()),
            Some("application/json"),
        );
    }

    #[test]
    fn tunnel_rejects_client_supplied_auth_or_device_headers() {
        for forbidden in ["authorization", DEVICE_ID_HEADER] {
            let err = validate_request_head(&head(vec![TunnelHeader::new(forbidden, "value")]))
                .unwrap_err();
            assert_eq!(err, (400, "forbidden tunnel header"));
        }
    }

    #[test]
    fn only_an_api_leg_advertises_reuse() {
        assert_eq!(
            reuse_for(LegClass::Api),
            Some(TunnelReuse { idle_ms: 60_000 })
        );
        // A blob transfer runs to 100 MiB / ten minutes: pooling one would block
        // every API call behind it, and the relay meters it as background
        // bandwidth. Enforced HERE, not trusted to the client.
        assert_eq!(reuse_for(LegClass::Blob), None);
        assert_eq!(reuse_for(LegClass::Chat), None);
    }
}

/// The leg as a whole: one Noise IK handshake, then N requests over a duplex that
/// stands in for a spliced relay data leg. These pin the framing rule — the thing
/// a one-shot leg used to get for free.
#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::device::load_or_create_static_keypair;
    use crate::test_support::build_test_deps;
    use baybo_store::{DeviceRow, DeviceStatus};
    use device_proto::noise::{NOISE_MAX_MESSAGE, StaticKeypair};
    use snow::{HandshakeState, TransportState};
    use tokio::sync::mpsc;

    const TEST_TOKEN: &str = "device-auth-token-fixed-0123456789abcdef";

    fn signing_key_for_tests() -> device_proto::delegation::SigningKey {
        device_proto::delegation::generate_signing_key()
    }

    struct ChanSink(mpsc::Sender<Vec<u8>>);
    struct ChanSource(mpsc::Receiver<Vec<u8>>);

    #[async_trait::async_trait]
    impl BinarySink for ChanSink {
        async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
            self.0.send(bytes).await.map_err(|_| ())
        }
        async fn close(&mut self) {}
    }

    #[async_trait::async_trait]
    impl BinarySource for ChanSource {
        async fn next_bytes(&mut self) -> Option<Vec<u8>> {
            self.0.recv().await
        }
    }

    /// The phone half of a tunnel leg: the IK initiator, plus seal/open over the
    /// same chunked Noise framing the real client uses.
    struct Phone {
        tx: mpsc::Sender<Vec<u8>>,
        rx: mpsc::Receiver<Vec<u8>>,
        transport: TransportState,
        reassembler: FrameReassembler,
        pending: VecDeque<TunnelResponse>,
        session: tokio::task::JoinHandle<Result<(), RelaySessionError>>,
    }

    impl Phone {
        async fn send(&mut self, req: &TunnelRequest) {
            let plaintext = api_tunnel::encode(req).expect("encode");
            for message in write_chunked(&mut self.transport, &plaintext).expect("seal") {
                self.tx.send(message).await.expect("leg alive");
            }
        }

        /// Next response frame, or `None` once the gateway hung up.
        async fn recv(&mut self) -> Option<TunnelResponse> {
            loop {
                if let Some(res) = self.pending.pop_front() {
                    return Some(res);
                }
                let bytes = self.rx.recv().await?;
                for frame in self
                    .reassembler
                    .read(&mut self.transport, &bytes)
                    .expect("open")
                {
                    self.pending
                        .push_back(api_tunnel::decode(&frame).expect("decode"));
                }
            }
        }

        /// Drive one whole GET and return its head plus the assembled body.
        async fn get(&mut self, id: u64, path: &str) -> (TunnelResponse, Vec<u8>) {
            self.send(&TunnelRequest::Head {
                request_id: id,
                method: "GET".into(),
                path: path.into(),
                headers: vec![],
                body_len: None,
            })
            .await;
            self.read_response(id).await
        }

        async fn read_response(&mut self, id: u64) -> (TunnelResponse, Vec<u8>) {
            let head = self.recv().await.expect("a response head");
            let body_len = match &head {
                TunnelResponse::Head {
                    request_id,
                    body_len,
                    ..
                } => {
                    assert_eq!(*request_id, id, "a response must answer its own request");
                    body_len.unwrap_or(0)
                }
                // An Error head has no body to collect.
                _ => return (head, Vec::new()),
            };
            let mut body = Vec::new();
            while (body.len() as u64) < body_len {
                match self.recv().await.expect("a body frame") {
                    TunnelResponse::Body { data, last, .. } => {
                        body.extend(data);
                        if last {
                            break;
                        }
                    }
                    other => panic!("expected a body frame, got {other:?}"),
                }
            }
            (head, body)
        }

        /// Hang up and report how the gateway's session ended.
        async fn close(mut self) -> Result<(), RelaySessionError> {
            drop(self.tx);
            tokio::time::timeout(Duration::from_secs(5), &mut self.session)
                .await
                .expect("the session must not sit on its idle budget after the peer closes")
                .expect("session task")
        }

        /// Whether the gateway closed the leg on its own.
        async fn hung_up(&mut self) -> bool {
            tokio::time::timeout(Duration::from_secs(5), self.recv())
                .await
                .expect("the gateway must not hang")
                .is_none()
        }
    }

    async fn connect(class: LegClass) -> Phone {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        // Two keys, as on a real device: the Noise static that the responder
        // resolves the leg by, and the ed25519 identity the `device-<hex>` id is
        // derived from — the admin auth middleware re-derives a pubkey from that
        // id, so a made-up string is a 400 before any route runs.
        let device = StaticKeypair::generate().expect("device key");
        let device_id =
            device_proto::delegation::device_id_for(&signing_key_for_tests().verifying_key());
        tg.deps
            .stores
            .device
            .create(&DeviceRow {
                device_id: device_id.clone(),
                device_pubkey: device.public().to_vec(),
                auth_token_sha256: baybo_store::device::hash_auth_token(TEST_TOKEN),
                status: DeviceStatus::Approved,
                rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
                created_at: 0,
                approved_at: Some(0),
                last_seen_at: None,
                relay_url: "wss://relay.test".into(),
                push_url: "https://push.test".into(),
                remote_api_key: "inst-test".into(),
            })
            .await
            .expect("seed approved device row");
        let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
            .await
            .expect("gateway static key");
        let gw_pub = gw_static.public();
        let state = WsChannelState::from_deps(&tg.deps);

        let (gw_tx, mut phone_rx) = mpsc::channel::<Vec<u8>>(32);
        let (phone_tx, gw_rx) = mpsc::channel::<Vec<u8>>(32);
        let session = tokio::spawn(async move {
            // `tg` owns the stores; keep it alive for the session's lifetime.
            let _tg = tg;
            run_tunnel_session(ChanSink(gw_tx), ChanSource(gw_rx), class, &state).await
        });

        let mut hs: HandshakeState = device.ik_initiator(&gw_pub).expect("ik initiator");
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = hs.write_message(&[], &mut buf).expect("msg1");
        phone_tx.send(buf[..n].to_vec()).await.expect("send msg1");
        let msg2 = phone_rx.recv().await.expect("gateway msg2");
        hs.read_message(&msg2, &mut buf).expect("read msg2");

        Phone {
            tx: phone_tx,
            rx: phone_rx,
            transport: hs.into_transport_mode().expect("transport"),
            reassembler: FrameReassembler::new(),
            pending: VecDeque::new(),
            session,
        }
    }

    fn reuse_of(head: &TunnelResponse) -> Option<TunnelReuse> {
        match head {
            TunnelResponse::Head { reuse, .. } => *reuse,
            other => panic!("expected a head, got {other:?}"),
        }
    }

    fn status_of(head: &TunnelResponse) -> u16 {
        match head {
            TunnelResponse::Head { status, .. } | TunnelResponse::Error { status, .. } => *status,
            other => panic!("expected a head or an error, got {other:?}"),
        }
    }

    /// THE point of the change: one handshake, two requests. Each response answers
    /// its own id, and both advertise the reuse budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_sequential_requests_run_on_one_leg() {
        let mut phone = connect(LegClass::Api).await;

        let (first, body) = phone.get(1, "/v1/chat/sessions").await;
        assert_eq!(status_of(&first), 200, "{}", String::from_utf8_lossy(&body));
        assert_eq!(reuse_of(&first), Some(TunnelReuse { idle_ms: 60_000 }));
        assert!(!body.is_empty(), "the session list is JSON");

        let (second, _) = phone.get(2, "/v1/chat/sessions").await;
        assert_eq!(status_of(&second), 200);
        assert_eq!(reuse_of(&second), Some(TunnelReuse { idle_ms: 60_000 }));

        phone.close().await.expect("a clean close");
    }

    /// A blob leg answers exactly one request and never advertises reuse, so a
    /// client cannot pool it even by accident — and it keeps today's shape, with no
    /// lifetime cap to guillotine a ten-minute upload.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blob_leg_serves_one_request_and_never_advertises_reuse() {
        let mut phone = connect(LegClass::Blob).await;

        let (head, _) = phone.get(1, "/v1/chat/sessions").await;
        assert_eq!(status_of(&head), 200);
        assert_eq!(reuse_of(&head), None, "a blob leg is never poolable");

        assert!(phone.hung_up().await, "and it closes after its one request");
    }

    /// An old client sends one request and hangs up. The gateway must leave at once
    /// rather than sit on the 60s idle budget waiting for a request that will never
    /// come — this is the old-app/new-gateway skew direction.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_that_closes_after_one_request_exits_promptly() {
        let mut phone = connect(LegClass::Api).await;
        let (head, _) = phone.get(1, "/v1/chat/sessions").await;
        assert_eq!(status_of(&head), 200);

        phone
            .close()
            .await
            .expect("EOF after a served request is a clean end, not an error");
    }

    /// Every shipped client encodes an empty body as `Head{body_len: Some(0)}` plus
    /// a `Body{last: true}` that the no-body branch never reads. On a one-shot leg
    /// the close reclaimed it; on a reused one it surfaces as the NEXT request. It
    /// must be dropped, and the next request must still get its own answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_legacy_empty_body_frame_does_not_poison_the_next_request() {
        let mut phone = connect(LegClass::Api).await;

        phone
            .send(&TunnelRequest::Head {
                request_id: 1,
                method: "POST".into(),
                path: "/v1/chat/sessions".into(),
                headers: vec![TunnelHeader::new("content-type", "application/json")],
                body_len: Some(0),
            })
            .await;
        phone
            .send(&TunnelRequest::Body {
                request_id: 1,
                offset: 0,
                data: Vec::new(),
                last: true,
            })
            .await;
        let (first, _) = phone.read_response(1).await;
        assert!((200..300).contains(&status_of(&first)));

        let (second, body) = phone.get(2, "/v1/chat/sessions").await;
        assert_eq!(status_of(&second), 200);
        assert!(
            !body.is_empty(),
            "the straggler must not have been read as request 2's body"
        );

        phone.close().await.expect("a clean close");
    }

    /// The router can answer without ever reading the request body (an unrouted
    /// POST is 404'd before any extractor runs). We stop reading; the peer does
    /// not stop writing. The response is still delivered, and then the leg MUST
    /// close — its framing is no longer knowable.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_router_that_hangs_up_on_the_body_closes_the_leg() {
        let mut phone = connect(LegClass::Api).await;

        // More chunks than the forward channel's buffer, so at least one send has to
        // reach a receiver the router has already dropped.
        const CHUNKS: u64 = 12;
        phone
            .send(&TunnelRequest::Head {
                request_id: 1,
                method: "POST".into(),
                path: "/v1/definitely-not-a-route".into(),
                headers: vec![TunnelHeader::new("content-type", "application/json")],
                body_len: Some(CHUNKS),
            })
            .await;
        for offset in 0..CHUNKS {
            phone
                .send(&TunnelRequest::Body {
                    request_id: 1,
                    offset,
                    data: vec![b'x'],
                    last: offset + 1 == CHUNKS,
                })
                .await;
        }

        let (head, _) = phone.read_response(1).await;
        assert_eq!(status_of(&head), 404, "the client still gets its answer");
        assert_eq!(
            reuse_of(&head),
            None,
            "and a leg we are about to close must not advertise reuse — the client \
             reads NOTHING else, so it would park a corpse and serve the next call from it"
        );
        assert!(
            phone.hung_up().await,
            "but the leg cannot survive a body it never drained"
        );
    }

    /// A request whose head is rejected outright gets a `TunnelResponse::Error` —
    /// which carries no `reuse` field and therefore cannot say "keep going". Every
    /// Error means the leg is dead, so the client's rule stays one line.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_head_that_fails_validation_closes_the_leg() {
        let mut phone = connect(LegClass::Api).await;

        phone
            .send(&TunnelRequest::Head {
                request_id: 1,
                method: "GET".into(),
                path: "/internal/metrics".into(), // not under /v1
                headers: vec![],
                body_len: None,
            })
            .await;

        let err = phone.recv().await.expect("an error response");
        assert!(
            matches!(err, TunnelResponse::Error { status: 403, .. }),
            "{err:?}"
        );
        assert!(phone.hung_up().await);
    }

    /// A body for an id we never issued a head for: we cannot know how much of it is
    /// coming, so the stream is unreadable from here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_body_for_an_unknown_request_id_closes_the_leg() {
        let mut phone = connect(LegClass::Api).await;

        phone
            .send(&TunnelRequest::Body {
                request_id: 7,
                offset: 0,
                data: vec![1, 2, 3],
                last: true,
            })
            .await;

        let err = phone.recv().await.expect("an error response");
        assert!(
            matches!(err, TunnelResponse::Error { status: 400, .. }),
            "{err:?}"
        );
        assert!(phone.hung_up().await);
    }

    /// Ids must strictly increase. A repeat is a replay or a client whose framing we
    /// no longer understand — and it is exactly what would let a stale response be
    /// matched to the wrong request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_repeated_request_id_closes_the_leg() {
        let mut phone = connect(LegClass::Api).await;

        let (first, _) = phone.get(1, "/v1/chat/sessions").await;
        assert_eq!(status_of(&first), 200);

        phone
            .send(&TunnelRequest::Head {
                request_id: 1,
                method: "GET".into(),
                path: "/v1/chat/sessions".into(),
                headers: vec![],
                body_len: None,
            })
            .await;

        let err = phone.recv().await.expect("an error response");
        assert!(
            matches!(err, TunnelResponse::Error { status: 400, .. }),
            "{err:?}"
        );
        assert!(phone.hung_up().await);
    }
}
