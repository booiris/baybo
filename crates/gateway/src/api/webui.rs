//! Serves the React dashboard (built from `web/`) baked into the
//! gateway binary at compile time.
//!
//! `build.rs` walks `web/dist/` and emits a `lookup()` function where
//! each arm is an `include_bytes!`-backed asset — no embedding crate
//! needed at runtime. If the frontend hasn't been built, `build.rs`
//! drops a placeholder `index.html` so `cargo build` still works;
//! release builds run `npm ci && npm run build` in `web/` first to
//! ship the real dashboard.
//!
//! Mounted as the admin router fallback, so `/`, `/assets/…`, and any
//! unmatched path resolve here while `/healthz`, `/readyz`, and
//! `/v1/*` keep their explicit handlers. Unauthenticated on purpose —
//! the bundle is inert HTML/JS; every privileged data path still goes
//! through `/v1/*`, which keeps its bearer-token gate.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/webui_assets.rs"));
}

pub async fn serve(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };

    if let Some((bytes, mime)) = assets::lookup(path) {
        return build_response(bytes, mime);
    }

    // Unknown path — fall back to index.html so SPA deep links keep
    // working even if we later move off HashRouter.
    match assets::lookup("index.html") {
        Some((bytes, mime)) => build_response(bytes, mime),
        None => (StatusCode::NOT_FOUND, "webui bundle not embedded").into_response(),
    }
}

fn build_response(bytes: &'static [u8], mime: &'static str) -> Response {
    let content_type = HeaderValue::from_str(mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_index_for_root() {
        let uri: Uri = "/".parse().expect("root uri parses");
        let response = serve(uri).await;
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .expect("content-type header set");
        assert!(
            content_type
                .to_str()
                .map(|v| v.starts_with("text/html"))
                .unwrap_or(false),
            "root should serve text/html, got {content_type:?}"
        );
    }

    #[tokio::test]
    async fn unknown_path_falls_back_to_index() {
        let uri: Uri = "/nope/nested/path".parse().expect("uri parses");
        let response = serve(uri).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
}
