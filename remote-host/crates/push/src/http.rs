//! The `/notify` HTTP surface — a thin axum wrapper over [`NotifyService`].
//!
//! A gateway (A) `POST`s a [`NotifyRequest`] JSON body; the handler runs the
//! blind pipeline and maps the outcome to a status. The wall clock is read here
//! (the edge), keeping [`NotifyService`] clock-injected and deterministic.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};

use crate::notify::{
    NotifyOutcome, NotifyRequest, NotifyService, RegisterOutcome, RegisterRequest,
};

/// Shared state for the push HTTP server.
#[derive(Clone)]
pub struct PushState {
    pub service: Arc<NotifyService>,
}

/// Build the push router (`POST /notify`, `POST /register`).
pub fn router(state: PushState) -> Router {
    Router::new()
        .route(remote_host_protocol::push::NOTIFY, post(notify))
        .route(remote_host_protocol::push::REGISTER, post(register))
        .with_state(state)
}

async fn register(State(state): State<PushState>, Json(req): Json<RegisterRequest>) -> StatusCode {
    match state.service.register(req) {
        RegisterOutcome::Registered => StatusCode::OK,
        RegisterOutcome::Unadmitted => StatusCode::UNAUTHORIZED,
        // The delegation chain / signature / counter didn't verify.
        RegisterOutcome::Rejected => StatusCode::FORBIDDEN,
    }
}

async fn notify(State(state): State<PushState>, Json(req): Json<NotifyRequest>) -> StatusCode {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match state.service.notify(req, now).await {
        // A pruned token is still a successful "we handled it" from A's view.
        NotifyOutcome::Delivered | NotifyOutcome::Pruned => StatusCode::OK,
        NotifyOutcome::Unadmitted => StatusCode::UNAUTHORIZED,
        NotifyOutcome::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        NotifyOutcome::UnknownDevice => StatusCode::NOT_FOUND,
        // Signature / delegation / replay-counter verification failed.
        NotifyOutcome::Rejected => StatusCode::FORBIDDEN,
        NotifyOutcome::Failed(_) => StatusCode::BAD_GATEWAY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apns::{ApnsEnv, ApnsOutcome, ApnsRequest, ApnsSender};
    use crate::delegation::{ENV_SANDBOX, test_sign};
    use crate::jwt::ApnsProviderToken;
    use crate::store::{
        Admission, DeviceRegistration, DeviceTokenStore, InMemoryAdmission,
        InMemoryDeviceTokenStore,
    };
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, header};
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use tower::ServiceExt;

    const TEST_P8: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgPFauT/kbqwIxcoQW
BNxFLAfYXAa3OFmTIx3IcGqjUkyhRANCAATGtaYrLt8AL8cs25DIa+OeV4PCpUHt
SYW9s/UKX8shed4rIxRqMe3POJIY7OsF06EEtnyLrMjJg53H5HWAe2Mh
-----END PRIVATE KEY-----"#;

    struct OkApns;
    #[async_trait]
    impl ApnsSender for OkApns {
        async fn send(&self, _req: ApnsRequest) -> ApnsOutcome {
            ApnsOutcome::Delivered
        }
    }

    fn device() -> SigningKey {
        test_sign::signing_key(1)
    }
    fn gateway() -> SigningKey {
        test_sign::signing_key(2)
    }
    fn device_id() -> String {
        test_sign::device_id_for(&device().verifying_key())
    }
    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn app() -> Router {
        let store = InMemoryDeviceTokenStore::new();
        store.register(
            &device_id(),
            DeviceRegistration {
                apns_token: "tok".into(),
                env: ApnsEnv::Sandbox,
                gateway_pubkey: gateway().verifying_key().to_bytes(),
                last_counter: 0,
            },
        );
        let admission: Arc<dyn Admission> = Arc::new(InMemoryAdmission::with_keys(["inst-A"]));
        let service = Arc::new(NotifyService::new(
            admission,
            Arc::new(store),
            Arc::new(OkApns),
            Arc::new(ApnsProviderToken::new("KID", "TEAM", TEST_P8.as_bytes()).unwrap()),
            "com.baybo.app",
        ));
        router(PushState { service })
    }

    async fn post_notify(body: serde_json::Value) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri("/notify")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        app().oneshot(req).await.unwrap().status()
    }

    fn notify_body(instance: &str, device_id: &str, counter: u64) -> serde_json::Value {
        let (enc, n) = ("Y2lwaGVy", "bm9uY2U=");
        let sig = test_sign::sign_notify(&gateway(), device_id, enc, n, device_id, counter);
        serde_json::json!({
            "remote_api_key": instance,
            "device_id": device_id,
            "collapse_id": "c",
            "bid": device_id,
            "enc": enc,
            "n": n,
            "sig": b64(&sig.to_bytes()),
            "counter": counter,
        })
    }

    async fn post_register(body: serde_json::Value) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri("/register")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        app().oneshot(req).await.unwrap().status()
    }

    fn reg_body(instance: &str, counter: u64) -> serde_json::Value {
        let did = device_id();
        let deleg = test_sign::sign_delegation(&device(), &gateway().verifying_key());
        let sig = test_sign::sign_register(&gateway(), &did, "apns-tok-new", ENV_SANDBOX, counter);
        serde_json::json!({
            "remote_api_key": instance,
            "device_id": did,
            "apns_token": "apns-tok-new",
            "env": "sandbox",
            "gateway_pubkey": b64(&gateway().verifying_key().to_bytes()),
            "delegation": b64(&deleg.to_bytes()),
            "sig": b64(&sig.to_bytes()),
            "counter": counter,
        })
    }

    #[tokio::test]
    async fn register_admitted_returns_200_unadmitted_401() {
        assert_eq!(post_register(reg_body("inst-A", 1)).await, StatusCode::OK);
        assert_eq!(
            post_register(reg_body("nope", 1)).await,
            StatusCode::UNAUTHORIZED,
        );
    }

    #[tokio::test]
    async fn admitted_known_device_returns_200() {
        assert_eq!(
            post_notify(notify_body("inst-A", &device_id(), 1)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn unadmitted_instance_returns_401() {
        assert_eq!(
            post_notify(notify_body("nope", &device_id(), 1)).await,
            StatusCode::UNAUTHORIZED,
        );
    }

    #[tokio::test]
    async fn unknown_device_returns_404() {
        let other = test_sign::device_id_for(&test_sign::signing_key(5).verifying_key());
        assert_eq!(
            post_notify(notify_body("inst-A", &other, 1)).await,
            StatusCode::NOT_FOUND,
        );
    }

    #[tokio::test]
    async fn forged_signature_returns_403() {
        // A valid wire shape but a counter the signature doesn't cover → reject.
        let mut body = notify_body("inst-A", &device_id(), 1);
        body["counter"] = serde_json::json!(999);
        assert_eq!(post_notify(body).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn over_rate_returns_429() {
        // One shared router (cloned per request) keeps the limiter state across the
        // whole burst; each request carries a fresh counter (the replay floor).
        let app = app();
        let did = device_id();
        let post = |counter: u64| {
            Request::builder()
                .method("POST")
                .uri("/notify")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&notify_body("inst-A", &did, counter)).unwrap(),
                ))
                .unwrap()
        };
        for i in 1..=crate::ratelimit::NOTIFY_BURST as u64 {
            assert_eq!(
                app.clone().oneshot(post(i)).await.unwrap().status(),
                StatusCode::OK
            );
        }
        assert_eq!(
            app.clone()
                .oneshot(post(crate::ratelimit::NOTIFY_BURST as u64 + 1))
                .await
                .unwrap()
                .status(),
            StatusCode::TOO_MANY_REQUESTS,
        );
    }
}
