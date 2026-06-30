//! Bearer-token gate for the `/dashboard/api/*` subtree.
//!
//! The token is presented only via the `Authorization: Bearer …` header — never
//! a cookie — so CSRF is structurally impossible. Verification is constant-time
//! over the byte comparison (it leaks length only).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::DashState;

const BEARER_PREFIX: &str = "Bearer ";

/// A dashboard bearer token. Cloneable (cheap `Arc`) so the router state can
/// carry it. There is no length requirement — any non-empty value enables the
/// dashboard; the caller decides whether the dashboard is on at all.
#[derive(Clone)]
pub struct DashboardToken(Arc<String>);

impl DashboardToken {
    pub fn new(token: String) -> Self {
        Self(Arc::new(token))
    }

    /// Constant-time-ish equality (leaks length only).
    pub fn verify(&self, presented: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), presented.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut d = 0u8;
        for (x, y) in a.iter().zip(b) {
            d |= x ^ y;
        }
        d == 0
    }
}

/// `401 Unauthorized` for a missing or invalid bearer token.
fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
}

/// Middleware guarding the `/dashboard/api/*` routes.
pub(crate) async fn require_token(
    State(state): State<DashState>,
    req: Request,
    next: Next,
) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix(BEARER_PREFIX));
    match presented {
        Some(token) if state.token.verify(token) => next.run(req).await,
        _ => unauthorized(),
    }
}
