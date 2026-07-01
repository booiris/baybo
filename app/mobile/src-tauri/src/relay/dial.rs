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

/// Dial the blind relay's content-join leg for this pairing's `relay_node_id`,
/// retrying a transient `503` for a bounded window. `leg_class` tags the leg's class
/// header — `Some(LegClass::Blob)` meters it as background bandwidth and steers the
/// gateway's blob sub-protocol; `None` is the default chat class. Returns the raw
/// socket; the caller runs the leg's Noise handshake over it. Errors are prose (each
/// caller stringifies or wraps them for its own error surface).
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
                .insert(remote_host_protocol::relay::REMOTE_API_KEY_HEADER, value);
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
            Ok((ws, _)) => return Ok(ws),
            // The relay's `content_join` returns 503 while no gateway holds a live
            // control connection for this node; it re-dials the relay on a fixed
            // backoff, so retry briefly rather than failing the open.
            Err(WsError::Http(resp))
                if resp.status() == StatusCode::SERVICE_UNAVAILABLE && attempt < DIAL_RETRIES =>
            {
                attempt += 1;
                tokio::time::sleep(DIAL_RETRY_DELAY).await;
            }
            Err(WsError::Http(resp)) if resp.status() == StatusCode::SERVICE_UNAVAILABLE => {
                return Err(format!(
                    "gateway offline: {base} has no relay control connection; ensure the paired gateway is running with relay enabled"
                ));
            }
            Err(e) => return Err(format!("relay connect {base}: {e}")),
        }
    }
}
