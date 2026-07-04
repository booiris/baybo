//! Shared one-shot API tunnel leg helpers for relay `api` and `blob` calls.

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

pub(crate) const REQUEST_ID: u64 = 1;

const TUNNEL_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) async fn dial_tunnel_leg(
    record: &PairedRecord,
    local: &StaticKeypair,
    leg_class: remote_host_protocol::relay::LegClass,
) -> Result<(WsStream, ApiTunnelSession), String> {
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
    Ok((ws, session))
}

async fn recv_binary_with_timeout(ws: &mut WsStream, timeout: Duration) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, recv_binary(ws))
        .await
        .map_err(|_| "handshake timed out".to_string())?
}

async fn recv_binary(ws: &mut WsStream) -> Result<Vec<u8>, String> {
    crate::transport::recv_binary(ws)
        .await
        .map_err(|e| e.to_string())
}

pub(crate) async fn send_request(
    ws: &mut WsStream,
    session: &mut ApiTunnelSession,
    request: &TunnelRequest,
) -> Result<(), String> {
    for message in session
        .seal(request)
        .map_err(|e| format!("seal tunnel request: {e}"))?
    {
        ws.send(Message::Binary(message))
            .await
            .map_err(|e| format!("send tunnel request: {e}"))?;
    }
    Ok(())
}

pub(crate) async fn next_response(
    ws: &mut WsStream,
    session: &mut ApiTunnelSession,
) -> Result<TunnelResponse, String> {
    loop {
        let bytes = recv_binary(ws).await?;
        let responses = session
            .open(&bytes)
            .map_err(|e| format!("open tunnel response: {e}"))?;
        if let Some(response) = responses.into_iter().next() {
            return Ok(response);
        }
    }
}

pub(crate) async fn expect_response_head(
    ws: &mut WsStream,
    session: &mut ApiTunnelSession,
) -> Result<(u16, Vec<TunnelHeader>, Option<u64>), String> {
    loop {
        match next_response(ws, session).await? {
            TunnelResponse::Head {
                status,
                headers,
                body_len,
                ..
            } => return Ok((status, headers, body_len)),
            TunnelResponse::Error { status, reason, .. } => {
                return Err(format!("HTTP {status}: {reason}"));
            }
            TunnelResponse::Body { .. } => {}
        }
    }
}

pub(crate) async fn collect_response_body(
    ws: &mut WsStream,
    session: &mut ApiTunnelSession,
    body_len: Option<u64>,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    if body_len == Some(0) {
        return Ok(body);
    }
    loop {
        match next_response(ws, session).await? {
            TunnelResponse::Body {
                offset, data, last, ..
            } => {
                if offset != body.len() as u64 {
                    return Err(format!("body offset {offset} != expected {}", body.len()));
                }
                body.extend(data);
                if last {
                    return Ok(body);
                }
            }
            TunnelResponse::Error { status, reason, .. } => {
                return Err(format!("HTTP {status}: {reason}"));
            }
            TunnelResponse::Head { .. } => {}
        }
    }
}
