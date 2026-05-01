//! Convert rmcp `Content` arrays into [`ToolOutput`].
//!
//! Replaces the old `format_content` helper which silently elided every
//! non-text variant. Now `Image` content materialises into the
//! gateway's [`BlobStore`] and reaches a vision-capable model through
//! [`ToolOutput::MultiModalText`]. Audio / Resource / ResourceLink
//! still elide for now (no current consumer); add adapters when one
//! lands.

use std::sync::Arc;

use aura_storage::BlobStore;
use base64::Engine;
use rmcp::model::{Annotated, RawContent};

use crate::{ToolError, ToolOutput};

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
    let mut images: Vec<aura_model::ContentBlock> = Vec::new();

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
            RawContent::ResourceLink(_) => text_parts.push("[resource link elided]".into()),
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

async fn decode_image(
    b64: &str,
    mime_type: &str,
    blob_store: &Arc<dyn BlobStore>,
) -> crate::Result<aura_model::ContentBlock> {
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
    Ok(aura_model::ContentBlock::Image {
        blob: blob_ref,
        mime_type: mime.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_storage::test_support::MemoryBlobStore;
    use rmcp::model::{Annotated, RawImageContent, RawTextContent};

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
                    aura_model::ContentBlock::Image { mime_type, .. } if mime_type == "image/png"
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
