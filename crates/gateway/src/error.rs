use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, GatewayError>;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("bind to {addr} failed: {reason}")]
    Bind { addr: String, reason: String },

    #[error("auth token not initialized; run `baybo gateway enable`")]
    TokenMissing,

    #[error("vault error: {0}")]
    Vault(String),

    #[error("channel registry error: {0}")]
    Channels(String),

    #[error("session error: {0}")]
    Session(String),

    #[error("turn error: {0}")]
    Turn(String),

    #[error("cron error: {0}")]
    Cron(String),

    #[error("trace error: {0}")]
    Trace(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("not found: {0}")]
    NotFound(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    /// The request lost a race to a concurrent write and changed nothing. The
    /// stored record is intact and the same request may simply be retried —
    /// which is what separates this from a 500.
    #[error("conflict: {0}")]
    Conflict(String),

    #[error("internal: {0}")]
    Internal(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::TokenMissing => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = serde_json::json!({ "error": self.to_string() });
        (status, Json(body)).into_response()
    }
}
