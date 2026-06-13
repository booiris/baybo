//! Serves the bench viewer's React bundle, baked into the binary by
//! `build.rs` (zstd-compressed, lazily decompressed on first request).
//! Mounted as the router fallback so `/`, `/assets/…`, and SPA deep
//! links resolve here while `/api/*` keeps its explicit handlers.
//! Adapted from `crates/gateway/src/api/webui.rs`.

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

pub(crate) async fn serve(uri: Uri) -> Response {
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

    // Missing hashed asset → 404 (don't SPA-fallback, or a stale
    // `<script src>` masquerades as HTML and trips strict-MIME).
    if path.starts_with("assets/") {
        return (StatusCode::NOT_FOUND, "asset not found").into_response();
    }

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
    let cache_control = if path.starts_with("assets/") {
        HeaderValue::from_static("public, max-age=31536000, immutable")
    } else {
        HeaderValue::from_static("no-cache")
    };
    headers.insert(header::CACHE_CONTROL, cache_control);
    resp
}
