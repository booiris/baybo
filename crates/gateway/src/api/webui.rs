//! Serves the React dashboard (built from `web/`) baked into the
//! gateway binary at compile time.
//!
//! `build.rs` walks `app/web/dist/`, zstd-compresses each asset, and emits
//! a static asset table. The first request lazily decompresses that
//! table into memory — no embedding crate needed at runtime. If the
//! frontend hasn't been built, `build.rs` drops a placeholder
//! `index.html` so `cargo build` still works; release builds run
//! `pnpm install && pnpm --filter baybo-web build` first to ship the
//! real dashboard.
//!
//! Mounted as the admin router fallback, so `/`, `/assets/…`, and any
//! unmatched path resolve here while `/healthz`, `/readyz`, and
//! `/v1/*` keep their explicit handlers. Unauthenticated on purpose —
//! the bundle is inert HTML/JS; every privileged data path still goes
//! through `/v1/*`, which keeps its bearer-token gate.

use std::collections::HashMap;
use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/webui_assets.rs"));
}

struct DecompressedAsset {
    bytes: Box<[u8]>,
    mime: &'static str,
}

static DECOMPRESSED_ASSETS: OnceLock<Result<HashMap<&'static str, DecompressedAsset>, String>> =
    OnceLock::new();

pub async fn serve(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');
    let path = if raw.is_empty() { "index.html" } else { raw };

    match lookup(path) {
        Ok(Some((bytes, mime))) => return build_response(path, bytes, mime),
        Ok(None) => {}
        Err(message) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("webui asset load failed: {message}"),
            )
                .into_response();
        }
    }

    // A request that names a file gets a 404, never the SPA fallback. Handing
    // back `index.html` under someone else's name is how a stale
    // `<script src>` masquerades as HTML and trips the browser's strict-MIME
    // guard — and, since the PWA landed, how a bundle built with
    // `BAYBO_SKIP_WEBUI=1` would answer `/sw.js` and `/manifest.webmanifest`
    // with a page instead of admitting they aren't there.
    if names_a_file(path) {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    }

    // Unknown route — fall back to index.html so SPA deep links keep
    // working.
    match lookup("index.html") {
        Ok(Some((bytes, mime))) => build_response("index.html", bytes, mime),
        Ok(None) => (StatusCode::NOT_FOUND, "webui bundle not embedded").into_response(),
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("webui asset load failed: {message}"),
        )
            .into_response(),
    }
}

/// Whether the path's last segment carries an extension. SPA routes never do
/// (the dashboard is a `HashRouter`, so every real navigation is `/`), and the
/// build emits nothing extensionless.
fn names_a_file(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|segment| segment.contains('.'))
}

fn lookup(path: &str) -> Result<Option<(&'static [u8], &'static str)>, &'static str> {
    let assets = DECOMPRESSED_ASSETS.get_or_init(build_asset_cache);
    match assets {
        Ok(assets) => Ok(assets
            .get(path)
            .map(|asset| (asset.bytes.as_ref(), asset.mime))),
        Err(message) => Err(message.as_str()),
    }
}

fn build_asset_cache() -> Result<HashMap<&'static str, DecompressedAsset>, String> {
    let mut assets = HashMap::with_capacity(generated::ASSETS.len());
    for asset in generated::ASSETS {
        let bytes = zstd::stream::decode_all(asset.content_zst)
            .map_err(|e| format!("decompress {}: {e}", asset.path))?;
        assets.insert(
            asset.path,
            DecompressedAsset {
                bytes: bytes.into_boxed_slice(),
                mime: asset.mime,
            },
        );
    }
    Ok(assets)
}

fn build_response(path: &str, bytes: &'static [u8], mime: &'static str) -> Response {
    let content_type = HeaderValue::from_str(mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let mut resp = Response::new(Body::from(bytes));
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    // Hashed assets are fingerprinted — safe to cache forever. The
    // entry page must revalidate so rebuilds with a new bundle hash
    // take effect on the next load instead of waiting for the browser
    // heuristic-cache to expire. `/sw.js` rides that same branch, and must:
    // a cached worker script is a gateway upgrade the browser never notices.
    let cache_control = if path.starts_with("assets/") {
        HeaderValue::from_static("public, max-age=31536000, immutable")
    } else {
        HeaderValue::from_static("no-cache")
    };
    headers.insert(header::CACHE_CONTROL, cache_control);
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

    #[tokio::test]
    async fn missing_asset_returns_404_not_html() {
        let uri: Uri = "/assets/index-DEADBEEF.js".parse().expect("uri parses");
        let response = serve(uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// The PWA's entry points sit at the root, not under `assets/`. A build
    /// without a dashboard (`BAYBO_SKIP_WEBUI=1`) has neither, and answering
    /// them with `index.html` would register a page as a service worker.
    /// `robots.txt` stands in for them here because a locally-built `dist/`
    /// really does embed `sw.js` and `manifest.webmanifest`.
    #[tokio::test]
    async fn missing_root_file_returns_404_not_html() {
        let uri: Uri = "/robots.txt".parse().expect("uri parses");
        let response = serve(uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn only_extensionless_paths_are_spa_routes() {
        assert!(names_a_file("sw.js"));
        assert!(names_a_file("manifest.webmanifest"));
        assert!(names_a_file("assets/index-DEADBEEF.js"));
        assert!(!names_a_file("chat"));
        assert!(!names_a_file("traces/019826f0-1a2b-7c3d-8e4f-5a6b7c8d9e0f"));
    }

    #[tokio::test]
    async fn index_is_no_cache() {
        let uri: Uri = "/".parse().expect("root parses");
        let response = serve(uri).await;
        let cache = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            cache.contains("no-cache"),
            "expected no-cache, got {cache:?}"
        );
    }
}
