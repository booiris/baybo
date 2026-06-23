//! Convert rmcp `Content` arrays into [`ToolOutput`].
//!
//! Replaces the old `format_content` helper which silently elided every
//! non-text variant. Two image paths now reach a vision-capable model
//! through [`ToolOutput::MultiModalText`]:
//!
//!   1. **Inline `Image`** — base64 in the MCP frame. Decoded here,
//!      stored in the gateway's [`BlobStore`], and surfaced as a
//!      [`baybo_model::ContentBlock::Image`]. 16 MiB cap.
//!   2. **`ResourceLink` with an `baybo://blob/<id>` URI** — for the
//!      browser MCP server's large screenshots. Bytes already landed
//!      in the blob store via the sidecar's earlier
//!      `POST /v1/blobs`; the URI's path is the blob_id, so we just
//!      construct a [`baybo_model::BlobRef`] directly. No second
//!      download, no size cap (the cap rides on the upload endpoint).
//!
//! Other `ResourceLink` URI schemes are elided — handing arbitrary
//! `http://...` URIs to a fetcher would create a generic SSRF surface
//! every MCP server could exploit. Audio + EmbeddedResource also
//! elide for now (no current consumer).

use std::sync::Arc;

use base64::Engine;
use baybo_model::BlobRef;
use baybo_store::BlobStore;
use rmcp::model::{Annotated, RawContent};

use crate::{ToolError, ToolOutput};

/// URI scheme reserved for "this blob is already in the gateway's
/// BlobStore — here is its id". Embedded MCP servers (the browser
/// sidecar today) emit `baybo://blob/<sha256:hex>` after streaming
/// bytes via `POST /v1/blobs`. The adapter parses the path and
/// produces a [`baybo_model::BlobRef`] without a second fetch.
const BAYBO_BLOB_URI_PREFIX: &str = "baybo://blob/";

/// Hard cap on decoded image bytes per `Image` content part. Mirrors
/// the cap the deleted `browser_screenshot` enforced — well above any
/// plausible UI capture, well below per-message limits any
/// vision-capable LLM accepts.
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// Adapt a tool-call result's content into a [`ToolOutput`]. The
/// `is_error` discriminator from `CallToolResult` decides whether the
/// final variant is `Error` or one of the success variants.
pub async fn adapt_call_result(
    parts: &[Annotated<RawContent>],
    is_error: bool,
    blob_store: Option<&Arc<dyn BlobStore>>,
) -> crate::Result<ToolOutput> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut images: Vec<baybo_model::ContentBlock> = Vec::new();

    for part in parts {
        match &part.raw {
            RawContent::Text(t) => text_parts.push(t.text.clone()),
            RawContent::Image(img) => {
                let blob_store = blob_store.ok_or_else(|| {
                    ToolError::Execution(
                        "MCP server returned image content but no blob store is wired \
                         in — refusing to elide silently"
                            .to_string(),
                    )
                })?;
                let block = decode_image(&img.data, &img.mime_type, blob_store).await?;
                images.push(block);
            }
            RawContent::Audio(_) => text_parts.push("[audio content elided]".into()),
            RawContent::Resource(_) => text_parts.push("[resource content elided]".into()),
            RawContent::ResourceLink(link) => match parse_baybo_blob_uri(&link.uri) {
                Some(blob_id) => {
                    let mime = link
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".into());
                    images.push(baybo_model::ContentBlock::Image {
                        blob: BlobRef { blob_id },
                        mime_type: mime,
                    });
                }
                None => {
                    // Non-baybo:// URI — could be any http://, file://, …
                    // Fetching arbitrary URIs from MCP servers would be
                    // a generic SSRF surface, so we elide.
                    text_parts.push(format!("[external resource link elided: {}]", link.uri));
                }
            },
        }
    }

    let text = text_parts.join("\n");

    if is_error {
        // Errors collapse to a string (matches today's behavior). If
        // the server returned an Image inside an error result we drop
        // it: error text is what the agent loop reasons about.
        return Ok(ToolOutput::Error(if text.is_empty() {
            "(empty error)".to_string()
        } else {
            text
        }));
    }

    if images.is_empty() {
        return Ok(ToolOutput::Text(text));
    }

    let display_text = if text.is_empty() {
        "(image)".to_string()
    } else {
        text
    };
    Ok(ToolOutput::multi_modal_text(display_text, images))
}

/// Parse `baybo://blob/<blob_id>` and return the `<blob_id>` portion.
/// Returns `None` when the URI doesn't match the prefix or when the
/// blob_id segment is empty / contains a path separator (defensive
/// against a sidecar trying to navigate into an unrelated blob).
fn parse_baybo_blob_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix(BAYBO_BLOB_URI_PREFIX)?;
    if rest.is_empty() || rest.contains('/') || rest.contains('\\') {
        return None;
    }
    Some(rest.to_string())
}

async fn decode_image(
    b64: &str,
    mime_type: &str,
    blob_store: &Arc<dyn BlobStore>,
) -> crate::Result<baybo_model::ContentBlock> {
    // Reject upstream of base64 decode using the `b64.len() * 3 / 4`
    // upper bound so we never allocate a 16 MiB buffer just to discard
    // it. Same trick the deleted `browser_screenshot` used.
    let upper_bound = b64.len().saturating_mul(3) / 4;
    if upper_bound > MAX_IMAGE_BYTES {
        return Err(ToolError::Execution(format!(
            "MCP image content too large ({upper_bound} > {MAX_IMAGE_BYTES} bytes)",
        )));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| ToolError::Execution(format!("decode MCP image bytes: {e}")))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ToolError::Execution(format!(
            "MCP image content too large ({} > {MAX_IMAGE_BYTES} bytes)",
            bytes.len()
        )));
    }
    let mime = if mime_type.is_empty() {
        "application/octet-stream"
    } else {
        mime_type
    };
    let blob_ref = blob_store
        .put(&bytes, mime, None)
        .await
        .map_err(|e| ToolError::Execution(format!("store MCP image blob: {e}")))?;
    Ok(baybo_model::ContentBlock::Image {
        blob: blob_ref,
        mime_type: mime.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_storage::test_support::MemoryBlobStore;
    use rmcp::model::{Annotated, RawImageContent, RawResource, RawTextContent};

    fn text(s: &str) -> Annotated<RawContent> {
        Annotated {
            raw: RawContent::Text(RawTextContent {
                text: s.into(),
                meta: None,
            }),
            annotations: None,
        }
    }

    fn image(b64: &str, mime: &str) -> Annotated<RawContent> {
        Annotated {
            raw: RawContent::Image(RawImageContent {
                data: b64.into(),
                mime_type: mime.into(),
                meta: None,
            }),
            annotations: None,
        }
    }

    #[tokio::test]
    async fn pure_text_yields_text_output() {
        let parts = vec![text("hello"), text("world")];
        let out = adapt_call_result(&parts, false, None).await.unwrap();
        match out {
            ToolOutput::Text(s) => assert_eq!(s, "hello\nworld"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_flag_yields_error_output() {
        let parts = vec![text("oh no")];
        let out = adapt_call_result(&parts, true, None).await.unwrap();
        match out {
            ToolOutput::Error(s) => assert_eq!(s, "oh no"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn image_without_blob_store_errors() {
        let parts = vec![image("YQ==", "image/png")]; // base64 of "a"
        let err = adapt_call_result(&parts, false, None).await.unwrap_err();
        assert!(format!("{err}").contains("no blob store"));
    }

    #[tokio::test]
    async fn text_plus_image_yields_multi_modal() {
        let blob_store: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
        // 1x1 transparent PNG
        let png_b64 = base64::engine::general_purpose::STANDARD
            .encode([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        let parts = vec![text("here it is"), image(&png_b64, "image/png")];
        let out = adapt_call_result(&parts, false, Some(&blob_store))
            .await
            .unwrap();
        match out {
            ToolOutput::MultiModalText { text, llm_images } => {
                assert_eq!(text, "here it is");
                assert_eq!(llm_images.len(), 1);
                assert!(matches!(
                    &llm_images[0],
                    baybo_model::ContentBlock::Image { mime_type, .. } if mime_type == "image/png"
                ));
            }
            other => panic!("expected MultiModalText, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn image_only_yields_placeholder_text() {
        let blob_store: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
        let png_b64 = base64::engine::general_purpose::STANDARD.encode([0x89, 0x50, 0x4E, 0x47]);
        let parts = vec![image(&png_b64, "image/png")];
        let out = adapt_call_result(&parts, false, Some(&blob_store))
            .await
            .unwrap();
        match out {
            ToolOutput::MultiModalText { text, llm_images } => {
                assert_eq!(text, "(image)");
                assert_eq!(llm_images.len(), 1);
            }
            other => panic!("expected MultiModalText, got {other:?}"),
        }
    }

    fn resource_link(uri: &str, mime: Option<&str>) -> Annotated<RawContent> {
        Annotated {
            raw: RawContent::ResourceLink(RawResource {
                uri: uri.into(),
                name: "x".into(),
                title: None,
                description: None,
                mime_type: mime.map(str::to_owned),
                size: None,
                icons: None,
                meta: None,
            }),
            annotations: None,
        }
    }

    #[test]
    fn baybo_blob_uri_parser_accepts_well_formed() {
        assert_eq!(
            parse_baybo_blob_uri("baybo://blob/sha256:abc123"),
            Some("sha256:abc123".to_string()),
        );
    }

    #[test]
    fn baybo_blob_uri_parser_rejects_path_traversal() {
        // A sidecar embedding `..` in the blob id would otherwise let
        // it construct a BlobRef with whatever path component it
        // wants. Defensive check before the BlobStore lookup.
        assert!(parse_baybo_blob_uri("baybo://blob/sha256:..").is_some()); // colon ok
        assert!(parse_baybo_blob_uri("baybo://blob/foo/bar").is_none());
        assert!(parse_baybo_blob_uri("baybo://blob/foo\\bar").is_none());
        assert!(parse_baybo_blob_uri("baybo://blob/").is_none());
    }

    #[test]
    fn baybo_blob_uri_parser_rejects_other_schemes() {
        assert!(parse_baybo_blob_uri("http://gateway/v1/blobs/abc").is_none());
        assert!(parse_baybo_blob_uri("file:///tmp/x").is_none());
        assert!(parse_baybo_blob_uri("baybo://other/abc").is_none());
    }

    #[tokio::test]
    async fn baybo_blob_resource_link_becomes_image_block() {
        let blob_store: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
        let parts = vec![
            text("here it is"),
            resource_link("baybo://blob/sha256:deadbeef", Some("image/png")),
        ];
        let out = adapt_call_result(&parts, false, Some(&blob_store))
            .await
            .unwrap();
        match out {
            ToolOutput::MultiModalText { text, llm_images } => {
                assert_eq!(text, "here it is");
                assert_eq!(llm_images.len(), 1);
                match &llm_images[0] {
                    baybo_model::ContentBlock::Image { blob, mime_type } => {
                        assert_eq!(blob.blob_id, "sha256:deadbeef");
                        assert_eq!(mime_type, "image/png");
                    }
                    other => panic!("expected Image block, got {other:?}"),
                }
            }
            other => panic!("expected MultiModalText, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn external_resource_link_is_elided() {
        let blob_store: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
        let parts = vec![resource_link(
            "https://attacker.example/secret",
            Some("image/png"),
        )];
        let out = adapt_call_result(&parts, false, Some(&blob_store))
            .await
            .unwrap();
        // Elided to text — no fetch attempt, no image surfaced.
        match out {
            ToolOutput::Text(s) => assert!(s.contains("external resource link elided")),
            other => panic!("expected Text (elided), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversize_image_rejected_before_decode() {
        let blob_store: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
        // 25 MiB of base64 chars → ~18 MiB decoded → over cap
        let huge = "A".repeat(25 * 1024 * 1024);
        let parts = vec![image(&huge, "image/png")];
        let err = adapt_call_result(&parts, false, Some(&blob_store))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("too large"));
    }
}
