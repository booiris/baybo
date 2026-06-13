//! API error surface: a small enum that maps to HTTP status codes.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("invalid path")]
    BadPath,
    #[error("io: {0}")]
    Io(String),
    #[error("parse: {0}")]
    Parse(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = match self {
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::BadPath => StatusCode::BAD_REQUEST,
            ApiError::Io(_) | ApiError::Parse(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (code, self.to_string()).into_response()
    }
}
