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

use crate::notify::{NotifyOutcome, NotifyRequest, NotifyService};

/// Shared state for the push HTTP server.
#[derive(Clone)]
pub struct PushState {
    pub service: Arc<NotifyService>,
}

/// Build the push router (`POST /notify`).
pub fn router(state: PushState) -> Router {
    Router::new()
        .route("/notify", post(notify))
        .with_state(state)
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
        NotifyOutcome::UnknownDevice => StatusCode::NOT_FOUND,
        NotifyOutcome::Failed(_) => StatusCode::BAD_GATEWAY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apns::{ApnsEnv, ApnsOutcome, ApnsRequest, ApnsSender};
    use crate::jwt::ApnsProviderToken;
    use crate::store::{
        Admission, DeviceRegistration, DeviceTokenStore, InMemoryAdmission,
        InMemoryDeviceTokenStore,
    };
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::{Request, header};
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

    fn app() -> Router {
        let store = InMemoryDeviceTokenStore::new();
        store.register(
            "dev-1",
            DeviceRegistration {
                apns_token: "tok".into(),
                env: ApnsEnv::Sandbox,
            },
        );
        let admission: Arc<dyn Admission> = Arc::new(InMemoryAdmission::with_keys(["inst-A"]));
        let service = Arc::new(NotifyService::new(
            admission,
            Arc::new(store),
            Arc::new(OkApns),
            Arc::new(ApnsProviderToken::new("KID", "TEAM", TEST_P8.as_bytes()).unwrap()),
            "com.aura.app",
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

    fn body(instance: &str, device: &str) -> serde_json::Value {
        serde_json::json!({
            "instance_key": instance,
            "device_id": device,
            "collapse_id": "dev-1:sess-1",
            "kid": 0,
            "bid": "dev-1",
            "enc": "Y2lwaGVy",
            "n": "bm9uY2U=",
        })
    }

    #[tokio::test]
    async fn admitted_known_device_returns_200() {
        assert_eq!(post_notify(body("inst-A", "dev-1")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn unadmitted_instance_returns_401() {
        assert_eq!(
            post_notify(body("nope", "dev-1")).await,
            StatusCode::UNAUTHORIZED,
        );
    }

    #[tokio::test]
    async fn unknown_device_returns_404() {
        assert_eq!(
            post_notify(body("inst-A", "ghost")).await,
            StatusCode::NOT_FOUND,
        );
    }
}
