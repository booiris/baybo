//! The shared relay content-join dial: build the request, present the pairing's
//! admission key (and an optional leg-class header), and run the bounded `503`
//! retry loop that both the chat leg ([`super::chat`]) and each blob leg
//! ([`super::blob`]) use to reach the (possibly NAT'd) gateway through the blind
//! relay. The Noise handshake that follows differs per leg (a content session vs a
//! blob session), so it stays in each caller; only the dial is shared here.

use std::time::Duration;

use remote_host_protocol::relay::LegClass;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, http::StatusCode};

use super::pairing::PairedRecord;
use crate::transport::WsStream;

/// Retry budget for the content dial while the gateway's relay control link is
/// briefly absent. The gateway re-dials the relay on a fixed backoff after any drop
/// (5s `RECONNECT_BACKOFF` in the gateway's `relay_content`), so a phone that opens
/// a leg inside that window would otherwise get a hard error; this budget outlasts
/// it. Only a `503 gateway not connected` is retried — a permanent refusal (e.g.
/// `401` for an unadmitted key) surfaces at once.
const DIAL_RETRIES: usize = 14;
const DIAL_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Longest relay response-body excerpt carried into a log/error string — enough to
/// state the reject reason without dumping an arbitrary payload.
const HTTP_BODY_SNIPPET_MAX: usize = 120;

/// Log-friendly detail for a WS dial error: an HTTP upgrade reject surfaces its
/// status + response body (the relay's stated reason — the only way to tell a
/// 401 unadmitted-key reject from a 404 unknown-node from DNS/TLS noise);
/// everything else renders via `Display`.
pub(super) fn ws_error_detail(e: &WsError) -> String {
    let WsError::Http(resp) = e else {
        return e.to_string();
    };
    let status = resp.status();
    match resp
        .body()
        .as_deref()
        .map(|b| {
            String::from_utf8_lossy(b)
                .trim()
                .chars()
                .take(HTTP_BODY_SNIPPET_MAX)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty())
    {
        Some(body) => format!("http {status}: {body}"),
        None => format!("http {status}"),
    }
}

/// Dial the blind relay's content-join leg for this pairing's `relay_node_id`,
/// retrying a transient `503` for a bounded window. `leg_class` tags the leg's
/// class header — `Api` and `Blob` both steer the gateway's API tunnel, while
/// `Blob` also meters as background bandwidth; `None` is the default chat class.
/// Returns the raw socket; the caller runs the leg's Noise handshake over it.
/// Errors are prose (each caller stringifies or wraps them for its own error
/// surface).
pub(super) async fn dial_content_join(
    record: &PairedRecord,
    leg_class: Option<LegClass>,
) -> Result<WsStream, String> {
    if record.relay_url.is_empty() {
        return Err("no relay url for this pairing".into());
    }
    if record.relay_node_id.is_empty() {
        return Err("paired gateway has no relay route; re-pair".into());
    }
    let base = record.relay_url.trim_end_matches('/');
    let url = remote_host_protocol::relay::content_join_url(base, &record.relay_node_id);
    let leg = leg_class.unwrap_or_default().as_str();

    let mut attempt = 0usize;
    loop {
        // Rebuilt per attempt (`into_client_request` yields an owned request).
        // Present the admission key the QR carried at pairing — the relay admits the
        // phone leg too.
        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("bad relay url {base}: {e}"))?;
        if !record.remote_api_key.is_empty() {
            let value = record
                .remote_api_key
                .parse()
                .map_err(|e| format!("bad instance key header: {e}"))?;
            req.headers_mut()
                .insert(remote_host_protocol::REMOTE_API_KEY_HEADER, value);
        }
        if let Some(class) = leg_class {
            let value = class
                .as_str()
                .parse()
                .map_err(|e| format!("bad class header: {e}"))?;
            req.headers_mut()
                .insert(remote_host_protocol::relay::RELAY_LEG_CLASS_HEADER, value);
        }
        match connect_async(req).await {
            Ok((ws, _)) => {
                if attempt > 0 {
                    log::info!(
                        "relay content-join connected after {attempt} retries (node={} url={base} leg={leg})",
                        record.relay_node_id
                    );
                }
                return Ok(ws);
            }
            // The relay's `content_join` returns 503 while no gateway holds a live
            // control connection for this node; it re-dials the relay on a fixed
            // backoff, so retry briefly rather than failing the open.
            Err(WsError::Http(resp))
                if resp.status() == StatusCode::SERVICE_UNAVAILABLE && attempt < DIAL_RETRIES =>
            {
                attempt += 1;
                log::debug!(
                    "relay content-join 503, gateway control link absent; retrying (attempt {attempt}/{DIAL_RETRIES}, node={} url={base} leg={leg})",
                    record.relay_node_id
                );
                tokio::time::sleep(DIAL_RETRY_DELAY).await;
            }
            Err(WsError::Http(resp)) if resp.status() == StatusCode::SERVICE_UNAVAILABLE => {
                log::warn!(
                    "relay content-join failed: gateway offline after {DIAL_RETRIES} retries (node={} url={base} key_tag={} leg={leg})",
                    record.relay_node_id,
                    remote_host_protocol::key_tag(&record.remote_api_key)
                );
                return Err(format!(
                    "gateway offline: {base} has no relay control connection; ensure the paired gateway is running with relay enabled"
                ));
            }
            Err(e) => {
                log::warn!(
                    "relay content-join refused/failed: {} (node={} url={base} key_tag={} leg={leg} attempt={attempt})",
                    ws_error_detail(&e),
                    record.relay_node_id,
                    remote_host_protocol::key_tag(&record.remote_api_key)
                );
                return Err(format!("relay connect {base}: {e}"));
            }
        }
    }
}
