//! Relay-mode gateway API client over one-shot API tunnel legs.

use device_proto::noise::StaticKeypair;
use serde::de::DeserializeOwned;

use super::pairing::load_paired_record;
use super::tunnel::{
    REQUEST_ID, collect_response_body, dial_tunnel_leg, expect_response_head, send_request,
};
use crate::core::{MAX_TUNNEL_CHUNK, TunnelHeader, TunnelRequest};
use crate::gateway_api::GatewayJsonClient;

const HEADER_CONTENT_TYPE: &str = "content-type";

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
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, "application/json")],
                Some(body),
            )
            .await?;
            serde_json::from_slice(&body).map_err(|e| format!("decode response: {e}"))
        }
    }
}

async fn request(
    method: &str,
    path: &str,
    headers: Vec<TunnelHeader>,
    body: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let (mut ws, mut session) =
        dial_tunnel_leg(&record, &local, remote_host_protocol::relay::LegClass::Api).await?;
    let body_len = body.as_ref().map(|body| body.len() as u64);

    send_request(
        &mut ws,
        &mut session,
        &TunnelRequest::Head {
            request_id: REQUEST_ID,
            method: method.into(),
            path: path.into(),
            headers,
            body_len,
        },
    )
    .await?;

    if let Some(body) = body.as_deref() {
        let size = body.len() as u64;
        if body.is_empty() {
            send_request(
                &mut ws,
                &mut session,
                &TunnelRequest::Body {
                    request_id: REQUEST_ID,
                    offset: 0,
                    data: Vec::new(),
                    last: true,
                },
            )
            .await?;
        } else {
            let mut offset = 0u64;
            for chunk in body.chunks(MAX_TUNNEL_CHUNK) {
                let chunk_len = chunk.len() as u64;
                let last = offset + chunk_len >= size;
                send_request(
                    &mut ws,
                    &mut session,
                    &TunnelRequest::Body {
                        request_id: REQUEST_ID,
                        offset,
                        data: chunk.to_vec(),
                        last,
                    },
                )
                .await?;
                offset += chunk_len;
            }
        }
    }

    let (status, _headers, body_len) = expect_response_head(&mut ws, &mut session).await?;
    if !(200..300).contains(&status) {
        let _ = ws.close(None).await;
        return Err(format!("api request failed: HTTP {status}"));
    }
    let body = collect_response_body(&mut ws, &mut session, body_len).await?;
    let _ = ws.close(None).await;
    Ok(body)
}
