//! Relay-mode gateway API client over API tunnel legs.

use std::time::Duration;

use device_proto::noise::StaticKeypair;
use serde::de::DeserializeOwned;

use super::pairing::load_paired_record;
use super::tunnel::{
    LegError, LegIo, body_frames, content_length_header, declared_body_len, dial_tunnel_leg,
};
use crate::core::{TunnelHeader, TunnelRequest};
use crate::gateway_api::{GatewayJsonClient, MEDIA_TYPE_JSON};

const HEADER_CONTENT_TYPE: &str = "content-type";

/// Ceiling on one request/response exchange, once the leg is up. There was no
/// per-request budget at all before: a leg that went quiet mid-response simply
/// hung, forever, and took its caller with it.
const TUNNEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct GatewayApi;

#[allow(clippy::manual_async_fn)]
impl GatewayJsonClient for GatewayApi {
    fn get_json<'a, T>(
        &'a self,
        path: &'a str,
    ) -> impl std::future::Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let body = request("GET", path, Vec::new(), None).await?;
            serde_json::from_slice(&body).map_err(|e| format!("decode response: {e}"))
        }
    }

    fn post_json<'a, T>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let body = request(
                "POST",
                path,
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, MEDIA_TYPE_JSON)],
                Some(body),
            )
            .await?;
            serde_json::from_slice(&body).map_err(|e| format!("decode response: {e}"))
        }
    }

    fn post_empty<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
        async move {
            request(
                "POST",
                path,
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, MEDIA_TYPE_JSON)],
                Some(body),
            )
            .await?;
            Ok(())
        }
    }

    fn put_empty<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
        async move {
            request(
                "PUT",
                path,
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, MEDIA_TYPE_JSON)],
                Some(body),
            )
            .await?;
            Ok(())
        }
    }

    fn delete_empty<'a>(
        &'a self,
        path: &'a str,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
        async move {
            request("DELETE", path, Vec::new(), None).await?;
            Ok(())
        }
    }
}

/// Drive one request/response over a leg, leaving it DRAINED.
///
/// A non-2xx carries a body too, and abandoning it on the wire would hand those
/// bytes to whoever writes the next head. `chat_lookup_message`'s 404 is the
/// ordinary result of an outbox rebase, not an edge case — this is the common
/// path, not the sad one.
pub(crate) async fn exchange(
    leg: &mut LegIo,
    method: &str,
    path: &str,
    headers: Vec<TunnelHeader>,
    body: Option<&[u8]>,
) -> Result<Vec<u8>, LegError> {
    match tokio::time::timeout(
        TUNNEL_REQUEST_TIMEOUT,
        run_exchange(leg, method, path, headers, body),
    )
    .await
    {
        Ok(outcome) => outcome,
        // A leg that went quiet is not one we can reason about: whatever it still
        // owes us would arrive tagged with an id nobody is expecting.
        Err(_) => Err(LegError::dead("tunnel request timed out")),
    }
}

async fn run_exchange(
    leg: &mut LegIo,
    method: &str,
    path: &str,
    mut headers: Vec<TunnelHeader>,
    body: Option<&[u8]>,
) -> Result<Vec<u8>, LegError> {
    let request_id = leg.claim_request_id();
    let body_len = declared_body_len(body);
    if let Some(len) = body_len {
        headers.push(content_length_header(len));
    }

    leg.send(&TunnelRequest::Head {
        request_id,
        method: method.into(),
        path: path.into(),
        headers,
        body_len,
    })
    .await?;
    if let Some(body) = body {
        for frame in body_frames(request_id, body) {
            leg.send(&frame).await?;
        }
    }

    let head = leg.expect_response_head(request_id).await?;
    // Drain first, judge second.
    let response_body = leg.collect_response_body(request_id, head.body_len).await?;
    if !(200..300).contains(&head.status) {
        return Err(LegError::Http {
            status: head.status,
        });
    }
    Ok(response_body)
}

/// One request on its own leg: dial, exchange, hang up.
async fn request(
    method: &str,
    path: &str,
    headers: Vec<TunnelHeader>,
    body: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let mut leg =
        dial_tunnel_leg(&record, &local, remote_host_protocol::relay::LegClass::Api).await?;

    let outcome = exchange(&mut leg, method, path, headers, body.as_deref()).await;
    leg.close().await;
    outcome.map_err(String::from)
}
