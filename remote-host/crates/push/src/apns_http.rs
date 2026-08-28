//! The real HTTP/2 APNs sender (production impl of [`ApnsSender`]).
//!
//! POSTs the blind request to `https://<env-host>/3/device/<token>` over HTTP/2
//! (reqwest negotiates h2 via TLS ALPN, which APNs requires) with the standard
//! `apns-*` headers and the ES256 provider token. The live transport only runs
//! against Apple on a real device (M4); the URL building, header set, and APNs
//! status classification are pure and host-tested below.

use async_trait::async_trait;

use crate::apns::{ApnsEnvironment, ApnsOutcome, ApnsRequest, ApnsSender, host};

/// reqwest-backed APNs sender. One shared client (its connection pool keeps the
/// HTTP/2 connection to APNs warm across pushes).
pub struct HttpApnsSender {
    client: reqwest::Client,
}

impl HttpApnsSender {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for HttpApnsSender {
    fn default() -> Self {
        Self::new()
    }
}

/// The APNs request URL for an env + device token.
fn apns_url(environment: ApnsEnvironment, device_token: &str) -> String {
    format!("https://{}/3/device/{}", host(environment), device_token)
}

/// APNs "accepted" HTTP status.
const APNS_STATUS_OK: u16 = 200;

/// Map an APNs HTTP response to a normalized [`ApnsOutcome`]. APNs returns `200`
/// on accept; `410 Unregistered` (body carries a `timestamp` ms honored before
/// deleting) and `400 BadDeviceToken` mean prune; everything else is retryable.
fn classify(
    status: u16,
    reason: Option<&str>,
    timestamp_ms: Option<u64>,
    apns_id: Option<String>,
) -> ApnsOutcome {
    match status {
        APNS_STATUS_OK => ApnsOutcome::Delivered { apns_id },
        410 => ApnsOutcome::Unregistered {
            timestamp_ms: timestamp_ms.unwrap_or(0),
        },
        400 if reason == Some("BadDeviceToken") => ApnsOutcome::BadDeviceToken,
        other => ApnsOutcome::TransientError(match reason {
            Some(r) => format!("apns status {other}: {r}"),
            None => format!("apns status {other}"),
        }),
    }
}

/// Pull `{ "reason": ..., "timestamp": ... }` out of an APNs error body. APNs
/// only sends a body on failure; on `200` this is empty and both are `None`.
fn parse_error_body(body: &[u8]) -> (Option<String>, Option<u64>) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (None, None);
    };
    let reason = v.get("reason").and_then(|r| r.as_str()).map(str::to_owned);
    let timestamp_ms = v.get("timestamp").and_then(serde_json::Value::as_u64);
    (reason, timestamp_ms)
}

#[async_trait]
impl ApnsSender for HttpApnsSender {
    async fn send(&self, req: ApnsRequest) -> ApnsOutcome {
        let url = apns_url(req.environment, &req.device_token);
        let token_len = req.device_token.len();
        let resp = self
            .client
            .post(&url)
            .header("authorization", format!("bearer {}", req.provider_jwt))
            .header("apns-topic", &req.topic)
            .header("apns-push-type", "alert")
            .header("apns-priority", "10")
            .header("apns-collapse-id", &req.collapse_id)
            .body(req.payload)
            .send()
            .await;
        match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                // `apns-id` is Apple's per-notification trace id — the only handle
                // for escalating a delivery question to Apple.
                let apns_id = r
                    .headers()
                    .get("apns-id")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let (reason, timestamp_ms, body_read) = match r.bytes().await {
                    Ok(b) => {
                        let (reason, ts) = parse_error_body(&b);
                        (reason, ts, true)
                    }
                    Err(_) => (None, None, false),
                };
                if status != APNS_STATUS_OK {
                    tracing::debug!(
                        status,
                        reason = reason
                            .as_deref()
                            .unwrap_or(if body_read { "<none>" } else { "<unreadable body>" }),
                        apns_id = apns_id.as_deref().unwrap_or("<none>"),
                        environment = ?req.environment,
                        token_len,
                        "push: APNs response"
                    );
                }
                classify(status, reason.as_deref(), timestamp_ms, apns_id)
            }
            Err(e) => {
                // reqwest's Display embeds the request URL, whose path carries the
                // device token — strip it before the string is logged or bubbled.
                let e = e.without_url();
                tracing::warn!(
                    host = host(req.environment),
                    error = %e,
                    "push: APNs HTTP request failed (transport)"
                );
                ApnsOutcome::TransientError(e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_uses_the_env_host() {
        assert_eq!(
            apns_url(ApnsEnvironment::Production, "tok-1"),
            "https://api.push.apple.com/3/device/tok-1",
        );
        assert_eq!(
            apns_url(ApnsEnvironment::Sandbox, "tok-1"),
            "https://api.sandbox.push.apple.com/3/device/tok-1",
        );
    }

    #[test]
    fn classify_maps_each_apns_status() {
        assert_eq!(
            classify(APNS_STATUS_OK, None, None, Some("apns-id-1".into())),
            ApnsOutcome::Delivered {
                apns_id: Some("apns-id-1".into()),
            },
        );
        assert_eq!(
            classify(APNS_STATUS_OK, None, None, None),
            ApnsOutcome::Delivered { apns_id: None },
        );
        assert_eq!(
            classify(400, Some("BadDeviceToken"), None, None),
            ApnsOutcome::BadDeviceToken,
        );
        assert_eq!(
            classify(410, Some("Unregistered"), Some(1_700_000_000_000), None),
            ApnsOutcome::Unregistered {
                timestamp_ms: 1_700_000_000_000,
            },
        );
        // A 410 with no body timestamp still prunes (epoch 0).
        assert_eq!(
            classify(410, None, None, None),
            ApnsOutcome::Unregistered { timestamp_ms: 0 },
        );
        // A non-BadDeviceToken 400 and a 5xx are both transient (don't prune).
        assert!(matches!(
            classify(400, Some("PayloadTooLarge"), None, None),
            ApnsOutcome::TransientError(_),
        ));
        assert!(matches!(
            classify(503, None, None, None),
            ApnsOutcome::TransientError(_),
        ));
    }

    #[test]
    fn parse_error_body_extracts_reason_and_timestamp() {
        let (reason, ts) =
            parse_error_body(br#"{"reason":"Unregistered","timestamp":1700000000000}"#);
        assert_eq!(reason.as_deref(), Some("Unregistered"));
        assert_eq!(ts, Some(1_700_000_000_000));

        // A success (empty) body yields neither.
        assert_eq!(parse_error_body(b""), (None, None));
    }
}
