use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use parking_lot::Mutex;
use tower::ServiceExt;

use super::*;

const GOOD_TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Records each backend method called; returns canned data otherwise.
#[derive(Default)]
struct FakeBackend {
    calls: Mutex<Vec<String>>,
}

impl FakeBackend {
    fn record(&self, name: &str) {
        self.calls.lock().push(name.to_owned());
    }
}

#[async_trait::async_trait]
impl DashboardBackend for FakeBackend {
    async fn overview(&self) -> Result<OverviewSnapshot, DashboardError> {
        self.record("overview");
        Ok(OverviewSnapshot {
            keys_admitted: 3,
            devices_bound: 2,
            push_enabled: true,
            gateways_connected: 1,
            relay_legs_pending: 0,
            relay_conns_live: 4,
            relay_keys_tracked: 5,
            ip_entries_tracked: 6,
            build_version: "test".into(),
            started_at: "2026-06-30 00:00:00".into(),
            uptime_secs: 42,
            server_time: "2026-06-30T00:00:42Z".into(),
        })
    }

    async fn list_keys(&self) -> Result<Vec<KeyRow>, DashboardError> {
        self.record("list_keys");
        Ok(vec![KeyRow {
            id: 7,
            label: Some("alpha".into()),
            key_last4: "ab12".into(),
            max_conns: Some(4),
            max_bps: Some(1000),
            per_server_max_bps: None,
            expires_at: None,
            created_at: "2026-06-29 12:00:00".into(),
            conns_live: 2,
            expired: false,
        }])
    }

    async fn reveal_key(&self, _id: i64) -> Result<RevealedKey, DashboardError> {
        self.record("reveal_key");
        Ok(RevealedKey {
            remote_api_key: "rh_secret".into(),
        })
    }

    async fn admit_key(&self, _req: AdmitKeyRequest) -> Result<AdmitKeyOutcome, DashboardError> {
        self.record("admit_key");
        Ok(AdmitKeyOutcome {
            id: 9,
            remote_api_key: "rh_new".into(),
        })
    }

    async fn edit_key(&self, _id: i64, _req: EditKeyRequest) -> Result<(), DashboardError> {
        self.record("edit_key");
        Ok(())
    }

    async fn revoke_key(&self, _id: i64) -> Result<(), DashboardError> {
        self.record("revoke_key");
        Ok(())
    }

    async fn kick_key(&self, _id: i64) -> Result<KickOutcome, DashboardError> {
        self.record("kick_key");
        Ok(KickOutcome { kicked: 1 })
    }

    async fn traffic_relay(&self, range: Range) -> Result<RelayTrafficSeries, DashboardError> {
        self.record("traffic_relay");
        Ok(RelayTrafficSeries {
            range,
            bucket: BucketUnit::Hour,
            buckets: vec![],
            by_key: vec![],
            totals: RelayTotals {
                bytes_up: 0,
                bytes_down: 0,
                frames_up: 0,
                frames_down: 0,
            },
        })
    }

    async fn traffic_push(&self, range: Range) -> Result<PushTrafficSeries, DashboardError> {
        self.record("traffic_push");
        Ok(PushTrafficSeries {
            range,
            bucket: BucketUnit::Hour,
            buckets: vec![],
            by_device: vec![],
            totals: PushTotals { sends: 0, bytes: 0 },
        })
    }

    async fn traffic_ip(&self, range: Range) -> Result<IpTrafficSeries, DashboardError> {
        self.record("traffic_ip");
        Ok(IpTrafficSeries {
            range,
            bucket: BucketUnit::Hour,
            buckets: vec![],
            by_endpoint: vec![],
            by_ip: vec![],
            totals: IpTotals {
                requests: 0,
                bytes: 0,
            },
        })
    }

    async fn traffic_ip_endpoints(
        &self,
        ip: String,
        _range: Range,
    ) -> Result<IpEndpointBreakdown, DashboardError> {
        self.record("traffic_ip_endpoints");
        Ok(IpEndpointBreakdown {
            ip,
            endpoints: vec![],
            totals: IpTotals {
                requests: 0,
                bytes: 0,
            },
        })
    }

    async fn list_devices(&self) -> Result<Vec<DeviceRow>, DashboardError> {
        self.record("list_devices");
        Ok(vec![])
    }
}

fn app() -> Router {
    router(DashboardConfig {
        backend: Arc::new(FakeBackend::default()),
        token: DashboardToken::new(GOOD_TOKEN.to_owned()),
    })
}

fn req(method: Method, path: &str, bearer: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).expect("request")
}

async fn status_of(method: Method, path: &str, bearer: Option<&str>) -> StatusCode {
    app()
        .oneshot(req(method, path, bearer))
        .await
        .expect("response")
        .status()
}

#[tokio::test]
async fn index_serves_html_and_leaks_nothing() {
    let resp = app()
        .oneshot(req(Method::GET, MOUNT_PREFIX, None))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some(HTML_CT)
    );
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
    let body = String::from_utf8(bytes.to_vec()).expect("utf8");
    assert!(!body.contains("rh_secret"));
    assert!(body.contains(MOUNT_PREFIX));
}

#[tokio::test]
async fn css_and_js_assets_have_correct_content_types() {
    let css = app()
        .oneshot(req(Method::GET, &format!("{MOUNT_PREFIX}/app.css"), None))
        .await
        .expect("response");
    assert_eq!(css.status(), StatusCode::OK);
    assert_eq!(
        css.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some(CSS_CT)
    );

    let js = app()
        .oneshot(req(Method::GET, &format!("{MOUNT_PREFIX}/app.js"), None))
        .await
        .expect("response");
    assert_eq!(js.status(), StatusCode::OK);
    assert_eq!(
        js.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some(JS_CT)
    );
}

#[tokio::test]
async fn overview_auth_matrix() {
    let path = format!("{MOUNT_PREFIX}/api/overview");
    assert_eq!(
        status_of(Method::GET, &path, None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of(Method::GET, &path, Some("wrong")).await,
        StatusCode::UNAUTHORIZED
    );
    // good token via the helper carries the "Bearer " prefix → accepted.
    assert_eq!(
        status_of(Method::GET, &path, Some(GOOD_TOKEN)).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn raw_header_without_bearer_prefix_is_rejected() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("{MOUNT_PREFIX}/api/overview"))
                .header(header::AUTHORIZATION, GOOD_TOKEN)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn every_api_route_401s_without_token() {
    let routes: &[(Method, String)] = &[
        (Method::GET, format!("{MOUNT_PREFIX}/api/overview")),
        (Method::GET, format!("{MOUNT_PREFIX}/api/keys")),
        (Method::POST, format!("{MOUNT_PREFIX}/api/keys")),
        (Method::GET, format!("{MOUNT_PREFIX}/api/keys/7/reveal")),
        (Method::PATCH, format!("{MOUNT_PREFIX}/api/keys/7")),
        (Method::DELETE, format!("{MOUNT_PREFIX}/api/keys/7")),
        (Method::POST, format!("{MOUNT_PREFIX}/api/keys/7/kick")),
        (Method::GET, format!("{MOUNT_PREFIX}/api/traffic/relay")),
        (Method::GET, format!("{MOUNT_PREFIX}/api/traffic/push")),
        (Method::GET, format!("{MOUNT_PREFIX}/api/traffic/ip")),
        (
            Method::GET,
            format!("{MOUNT_PREFIX}/api/traffic/ip/1.2.3.4/endpoints"),
        ),
        (Method::GET, format!("{MOUNT_PREFIX}/api/devices")),
    ];
    for (method, path) in routes {
        assert_eq!(
            status_of(method.clone(), path, None).await,
            StatusCode::UNAUTHORIZED,
            "{method} {path} should 401 without a token"
        );
    }
}

#[tokio::test]
async fn traffic_route_with_token_is_ok() {
    assert_eq!(
        status_of(
            Method::GET,
            &format!("{MOUNT_PREFIX}/api/traffic/relay?range=7d"),
            Some(GOOD_TOKEN)
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn ip_endpoints_route_with_token_is_ok() {
    assert_eq!(
        status_of(
            Method::GET,
            &format!("{MOUNT_PREFIX}/api/traffic/ip/203.0.113.7/endpoints?range=24h"),
            Some(GOOD_TOKEN)
        )
        .await,
        StatusCode::OK
    );
}

#[test]
fn token_verify_matches_exact_only() {
    let token = DashboardToken::new(GOOD_TOKEN.to_owned());
    assert!(token.verify(GOOD_TOKEN), "exact token accepted");
    assert!(!token.verify("wrong"), "wrong token rejected");
    assert!(!token.verify(""), "empty token rejected");
}

#[tokio::test]
async fn root_redirects_to_dashboard() {
    let resp = app()
        .oneshot(req(Method::GET, "/", None))
        .await
        .expect("response");
    assert!(
        resp.status().is_redirection(),
        "GET / should 3xx, got {}",
        resp.status()
    );
    assert_eq!(
        resp.headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok()),
        Some(MOUNT_PREFIX)
    );
}

#[test]
fn range_serde_and_hours() {
    for (s, r, h) in [
        ("24h", Range::H24, 24u32),
        ("7d", Range::D7, 168),
        ("30d", Range::D30, 720),
        ("60d", Range::D60, 1440),
    ] {
        let json = format!("\"{s}\"");
        let parsed: Range = serde_json::from_str(&json).expect("range");
        assert_eq!(parsed, r);
        assert_eq!(serde_json::to_string(&r).expect("ser"), json);
        assert_eq!(r.hours(), h);
    }
}

#[test]
fn range_query_defaults_to_h24() {
    let q: RangeQuery = serde_json::from_str("{}").expect("query");
    assert_eq!(q.range, Range::H24);
}

#[test]
fn key_row_field_names() {
    let row = KeyRow {
        id: 7,
        label: Some("alpha".into()),
        key_last4: "ab12".into(),
        max_conns: Some(4),
        max_bps: None,
        per_server_max_bps: None,
        expires_at: None,
        created_at: "now".into(),
        conns_live: 2,
        expired: false,
    };
    let v: serde_json::Value = serde_json::to_value(&row).expect("value");
    for field in [
        "id",
        "label",
        "key_last4",
        "max_conns",
        "max_bps",
        "per_server_max_bps",
        "expires_at",
        "created_at",
        "conns_live",
        "expired",
    ] {
        assert!(v.get(field).is_some(), "missing field {field}");
    }
}

#[test]
fn traffic_and_device_field_names() {
    let relay = RelayBucket {
        t: "2026-06-30 00:00:00".into(),
        bytes_up: 1,
        bytes_down: 2,
        frames_up: 3,
        frames_down: 4,
    };
    let v = serde_json::to_value(&relay).expect("value");
    for field in ["t", "bytes_up", "bytes_down", "frames_up", "frames_down"] {
        assert!(v.get(field).is_some(), "missing field {field}");
    }

    let push = PushDeviceTotal {
        device_id: "d1".into(),
        sends: 5,
        bytes: 6,
    };
    let v = serde_json::to_value(&push).expect("value");
    for field in ["device_id", "sends", "bytes"] {
        assert!(v.get(field).is_some(), "missing field {field}");
    }

    let device = DeviceRow {
        device_id: "d1".into(),
        provider: "apns".into(),
        environment: Some("sandbox".into()),
        token_masked: "··cd".into(),
        gateway_pubkey_hex: "ab".into(),
        last_counter: 7,
        sends_total: 8,
        bytes_total: 9,
    };
    let v = serde_json::to_value(&device).expect("value");
    for field in [
        "device_id",
        "provider",
        "environment",
        "token_masked",
        "gateway_pubkey_hex",
        "last_counter",
        "sends_total",
        "bytes_total",
    ] {
        assert!(v.get(field).is_some(), "missing field {field}");
    }
}

#[test]
fn mount_prefix_is_dashboard() {
    assert_eq!(MOUNT_PREFIX, "/dashboard");
    assert!(INDEX_HTML.contains(MOUNT_PREFIX));
}
