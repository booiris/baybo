//! **dashboard** — the operator's **metadata-only** status surface for
//! `remote-host`.
//!
//! Blind by construction: it reports counts only — admitted instances, bound
//! device tokens, connected gateways, pending relay legs — and **never any
//! conversation content** (the relay carries Noise ciphertext; push carries an
//! opaque encrypted preview). The dashboard polls a [`MetadataProvider`] the
//! runtime implements over the real push/relay components, so this crate stays
//! decoupled and can't even reach plaintext.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

/// A point-in-time, content-free snapshot of operator metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    /// Per-instance admission keys currently allowed.
    pub instances_admitted: usize,
    /// Devices with a bound APNs token in the push store.
    pub devices_bound: usize,
    /// Gateways holding a live A↔C control connection.
    pub gateways_connected: usize,
    /// Relay legs parked waiting for a partner.
    pub relay_legs_pending: usize,
}

/// What the dashboard polls for a fresh snapshot. Implemented by the runtime
/// over the live push/relay components.
pub trait MetadataProvider: Send + Sync {
    fn snapshot(&self) -> DashboardSnapshot;
}

#[derive(Clone)]
struct DashState {
    provider: Arc<dyn MetadataProvider>,
}

/// Build the dashboard router (`GET /status` → the metadata snapshot as JSON).
pub fn router(provider: Arc<dyn MetadataProvider>) -> Router {
    Router::new()
        .route("/status", get(status))
        .with_state(DashState { provider })
}

async fn status(State(state): State<DashState>) -> Json<DashboardSnapshot> {
    Json(state.provider.snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    struct Fixed(DashboardSnapshot);
    impl MetadataProvider for Fixed {
        fn snapshot(&self) -> DashboardSnapshot {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn status_returns_the_metadata_snapshot() {
        let snap = DashboardSnapshot {
            instances_admitted: 3,
            devices_bound: 7,
            gateways_connected: 2,
            relay_legs_pending: 1,
        };
        let app = router(Arc::new(Fixed(snap.clone())));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let got: DashboardSnapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(got, snap);
    }
}
