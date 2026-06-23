//! The real HTTP/2 APNs sender (production impl of [`ApnsSender`]).
//!
//! POSTs the blind request to `https://<env-host>/3/device/<token>` over HTTP/2
//! (reqwest negotiates h2 via TLS ALPN, which APNs requires) with the standard
//! `apns-*` headers and the ES256 provider token. The live transport only runs
//! against Apple on a real device (M4); the URL building, header set, and APNs
//! status classification are pure and host-tested below.

use async_trait::async_trait;

use crate::apns::{ApnsEnv, ApnsOutcome, ApnsRequest, ApnsSender};

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
fn apns_url(env: ApnsEnv, device_token: &str) -> String {
    format!("https://{}/3/device/{}", env.host(), device_token)
}

/// Map an APNs HTTP response to a normalized [`ApnsOutcome`]. APNs returns `200`
/// on accept; `410 Unregistered` (body carries a `timestamp` ms honored before
/// deleting) and `400 BadDeviceToken` mean prune; everything else is retryable.
fn classify(status: u16, reason: Option<&str>, timestamp_ms: Option<u64>) -> ApnsOutcome {
    match status {
        200 => ApnsOutcome::Delivered,
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
    let reason = v
        .get("reason")
        .and_then(|r| r.as_str())
        .map(str::to_owned);
    let timestamp_ms = v.get("timestamp").and_then(serde_json::Value::as_u64);
    (reason, timestamp_ms)
}

#[async_trait]
impl ApnsSender for HttpApnsSender {
    async fn send(&self, req: ApnsRequest) -> ApnsOutcome {
        let url = apns_url(req.env, &req.device_token);
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
                let (reason, timestamp_ms) = match r.bytes().await {
                    Ok(b) => parse_error_body(&b),
                    Err(_) => (None, None),
                };
                classify(status, reason.as_deref(), timestamp_ms)
            }
            Err(e) => ApnsOutcome::TransientError(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_uses_the_env_host() {
        assert_eq!(
            apns_url(ApnsEnv::Production, "tok-1"),
            "https://api.push.apple.com/3/device/tok-1",
        );
        assert_eq!(
            apns_url(ApnsEnv::Sandbox, "tok-1"),
            "https://api.sandbox.push.apple.com/3/device/tok-1",
        );
    }

    #[test]
    fn classify_maps_each_apns_status() {
        assert_eq!(classify(200, None, None), ApnsOutcome::Delivered);
        assert_eq!(
            classify(400, Some("BadDeviceToken"), None),
            ApnsOutcome::BadDeviceToken,
        );
        assert_eq!(
            classify(410, Some("Unregistered"), Some(1_700_000_000_000)),
            ApnsOutcome::Unregistered {
                timestamp_ms: 1_700_000_000_000,
            },
        );
        // A 410 with no body timestamp still prunes (epoch 0).
        assert_eq!(
            classify(410, None, None),
            ApnsOutcome::Unregistered { timestamp_ms: 0 },
        );
        // A non-BadDeviceToken 400 and a 5xx are both transient (don't prune).
        assert!(matches!(
            classify(400, Some("PayloadTooLarge"), None),
            ApnsOutcome::TransientError(_),
        ));
        assert!(matches!(
            classify(503, None, None),
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
