//! Gateway responder for E2E API tunnel relay legs.
//!
//! `LegClass::Api` and `LegClass::Blob` both enter here. The relay meters `Api`
//! as interactive and `Blob` as background, but after the shared Noise IK device
//! authentication the plaintext is the same HTTP-shaped tunnel protocol.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use axum::body::{self, Body};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Uri};
use bytes::Bytes;
use device_proto::api_tunnel::{
    self, MAX_TUNNEL_CHUNK, TunnelHeader, TunnelRequest, TunnelResponse,
};
use device_proto::noise::{FrameReassembler, write_chunked};
use futures::StreamExt;
use snow::TransportState;
use tokio_stream::wrappers::ReceiverStream;
use tower::ServiceExt;

use super::blobs::MAX_BLOB_BYTES;
use super::device_content::{
    BinarySink, BinarySource, RelayWs, TungBinSink, TungBinSource, responder_handshake,
};
use super::state::WsChannelState;
use crate::auth::AuthedClient;

const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(20);
const TUNNEL_UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const TUNNEL_UPLOAD_CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const HEADER_CONTENT_LENGTH: &str = "content-length";
const PATH_BLOBS: &str = "/v1/blobs";
const PATH_BLOBS_PREFIX: &str = "/v1/blobs/";
const PATH_CHAT_SESSIONS: &str = "/v1/chat/sessions";
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
];

struct RequestHead {
    request_id: u64,
    method: String,
    path: String,
    headers: Vec<TunnelHeader>,
    body_len: Option<u64>,
}

pub(crate) async fn run_api_tunnel_over_relay(ws: RelayWs, state: &WsChannelState) {
    let (sink, source) = ws.split();
    if let Err(reason) = run_tunnel_session(TungBinSink(sink), TungBinSource(source), state).await {
        tracing::debug!(reason = %reason, "relay api tunnel aborted");
    }
}

async fn run_tunnel_session<Si: BinarySink, So: BinarySource>(
    mut sink: Si,
    mut source: So,
    state: &WsChannelState,
) -> Result<(), String> {
    let (mut transport, device_id) = responder_handshake(&mut sink, &mut source, state).await?;
    let mut reassembler = FrameReassembler::new();
    let mut pending = VecDeque::new();

    tracing::info!(
        device = %super::short_hash(&device_id),
        "device api tunnel established",
    );

    let first = tokio::time::timeout(
        TUNNEL_IDLE_TIMEOUT,
        next_request(&mut source, &mut transport, &mut reassembler, &mut pending),
    )
    .await
    .map_err(|_| "timed out waiting for tunnel request".to_string())??;

    let head = match first {
        Some(TunnelRequest::Head {
            request_id,
            method,
            path,
            headers,
            body_len,
        }) => RequestHead {
            request_id,
            method,
            path,
            headers,
            body_len,
        },
        Some(TunnelRequest::Cancel { reason, .. }) => {
            return Err(format!("request canceled: {reason}"));
        }
        Some(TunnelRequest::Body { request_id, .. }) => {
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
        None => return Err("peer closed before request head".into()),
    };

    if let Err((status, reason)) = validate_request_head(&head) {
        send_error(&mut sink, &mut transport, head.request_id, status, reason).await?;
        return Ok(());
    }

    if is_http_forward(&head) {
        handle_http_forward(
            &mut sink,
            &mut source,
            &mut transport,
            &mut reassembler,
            &mut pending,
            state,
            &device_id,
            head,
        )
        .await?;
        return Ok(());
    }

    send_error(
        &mut sink,
        &mut transport,
        head.request_id,
        404,
        "unsupported tunnel endpoint",
    )
    .await?;
    Ok(())
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
    device_id: &str,
    head: RequestHead,
) -> Result<(), String> {
    if is_blob_upload(&head) {
        return handle_http_upload_forward(
            sink,
            source,
            transport,
            reassembler,
            pending,
            state,
            device_id,
            head,
        )
        .await;
    }

    if request_body_len(&head) != 0 {
        send_error(
            sink,
            transport,
            head.request_id,
            400,
            "forwarded endpoint does not accept a request body",
        )
        .await?;
        return Ok(());
    }
    let req = match build_forward_request(&head, device_id, Body::empty()) {
        Ok(req) => req,
        Err((status, reason)) => {
            send_error(sink, transport, head.request_id, status, &reason).await?;
            return Ok(());
        }
    };
    let response = super::tunnel_http::router(state.admin_state.clone(), state.clone())
        .oneshot(req)
        .await
        .map_err(|e| format!("forward tunnel HTTP request: {e}"))?;
    send_http_response(sink, transport, head.request_id, response).await
}

#[allow(clippy::too_many_arguments)]
async fn handle_http_upload_forward<Si: BinarySink, So: BinarySource>(
    sink: &mut Si,
    source: &mut So,
    transport: &mut TransportState,
    reassembler: &mut FrameReassembler,
    pending: &mut VecDeque<TunnelRequest>,
    state: &WsChannelState,
    device_id: &str,
    head: RequestHead,
) -> Result<(), String> {
    let declared_len = match head.body_len {
        Some(len) => len,
        None => {
            send_error(
                sink,
                transport,
                head.request_id,
                411,
                "missing content length",
            )
            .await?;
            return Ok(());
        }
    };
    if declared_len > MAX_BLOB_BYTES as u64 {
        send_error(
            sink,
            transport,
            head.request_id,
            413,
            "blob exceeds size limit",
        )
        .await?;
        return Ok(());
    }

    let request_id = head.request_id;
    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(4);
    let req =
        match build_forward_request(&head, device_id, Body::from_stream(ReceiverStream::new(rx))) {
            Ok(req) => req,
            Err((status, reason)) => {
                close_forward_body(tx, reason.clone()).await;
                send_error(sink, transport, request_id, status, &reason).await?;
                return Ok(());
            }
        };

    let router = super::tunnel_http::router(state.admin_state.clone(), state.clone());
    let response_task = tokio::spawn(async move {
        router
            .oneshot(req)
            .await
            .map_err(|e| format!("forward tunnel HTTP request: {e}"))
    });
    if let Err((status, reason)) = stream_forward_body(
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
        response_task.abort();
        let _ = response_task.await;
        send_error(sink, transport, request_id, status, &reason).await?;
        return Ok(());
    }

    let response = response_task
        .await
        .map_err(|e| format!("forward tunnel HTTP task failed: {e}"))??;
    send_http_response(sink, transport, request_id, response).await
}

fn build_forward_request(
    head: &RequestHead,
    device_id: &str,
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
    match (header_content_len, head.body_len) {
        (Some(header_len), Some(body_len)) if header_len != body_len => {
            return Err((
                400,
                "content-length does not match tunnel body length".to_string(),
            ));
        }
        (Some(header_len), None) if header_len != 0 => {
            return Err((
                400,
                "content-length does not match tunnel body length".to_string(),
            ));
        }
        _ => {}
    }
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
    if let Some(body_len) = head.body_len {
        builder = builder.header(HEADER_CONTENT_LENGTH, body_len.to_string());
    }
    let mut req = builder
        .body(body)
        .map_err(|e| (400, format!("invalid forwarded request: {e}")))?;
    req.extensions_mut().insert(AuthedClient::Device {
        device_id: device_id.to_owned(),
    });
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
) -> Result<(), (u16, String)> {
    if declared_len == 0 {
        drop(tx);
        return Ok(());
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
                if tx.send(Ok(Bytes::from(data))).await.is_err() {
                    return Ok(());
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
    Ok(())
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
            },
        )
        .await?;
        return send_streaming_http_body(sink, transport, request_id, body, body_len).await;
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

fn is_http_forward(head: &RequestHead) -> bool {
    let path = head.path.split('?').next().unwrap_or(head.path.as_str());
    if head.method.eq_ignore_ascii_case("GET") {
        path == PATH_CHAT_SESSIONS || path.starts_with(PATH_BLOBS_PREFIX)
    } else if head.method.eq_ignore_ascii_case("POST") {
        path == PATH_BLOBS
    } else {
        false
    }
}

fn is_blob_upload(head: &RequestHead) -> bool {
    head.method.eq_ignore_ascii_case("POST")
        && head.path.split('?').next().unwrap_or(head.path.as_str()) == PATH_BLOBS
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
