use std::sync::Arc;

use async_trait::async_trait;
use aura_storage::BlobStore;
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object};
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

/// Hard cap on decoded screenshot bytes. A `full_page=true` capture
/// of a giant page would otherwise land as a single tokio frame and
/// be base64-decoded into one `Vec<u8>` before reaching the blob
/// store. 16 MiB is well above the size of any plausible UI capture
/// (a 10000×10000 PNG of typical web content rarely exceeds ~5 MiB)
/// and well below the per-message limits any vision-capable LLM
/// will accept anyway.
const MAX_SCREENSHOT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Params {
    #[serde(default)]
    full_page: bool,
}

pub struct BrowserScreenshotTool {
    client: Arc<dyn BrowserSidecarClient>,
    blob_store: Arc<dyn BlobStore>,
}

impl BrowserScreenshotTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>, blob_store: Arc<dyn BlobStore>) -> Self {
        Self { client, blob_store }
    }
}

#[async_trait]
impl Tool for BrowserScreenshotTool {
    fn name(&self) -> &str {
        "browser_screenshot"
    }

    fn description(&self) -> &str {
        "Capture a PNG screenshot of the current page and return it as an image \
         attachment the LLM can see on the next turn (provided the model supports \
         vision). Bytes are persisted in the blob store; both the LLM and the user's \
         channel receive a reference to the same blob.\n\n\
         `full_page=true` captures the full scrollable page; default captures only \
         the current viewport."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "full_page": {
                    "type": "boolean",
                    "description": "If true, capture the full scrollable page (default false: viewport only)"
                }
            }),
            &[],
        )
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        // Screenshot can capture whatever is on the active page —
        // including PII the user opened in a previous step (an email
        // tab, a banking dashboard, …) that they did not intend the
        // LLM to see. Always prompt. The cache key splits viewport
        // vs full-page so the user can grant the cheaper variant
        // without auto-approving the heavier exfil surface.
        let full = params.get("full_page").and_then(|v| v.as_bool()) == Some(true);
        let mode = if full { "full_page" } else { "viewport" };
        vec![ResourceAccess::ExecCommand {
            command: format!("browser_screenshot: {mode}"),
        }]
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let result = call_sidecar(
            &self.client,
            "screenshot",
            json!({ "full_page": p.full_page }),
            ctx,
        )
        .await?;

        let png_b64 = result
            .get("png_b64")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::Execution(
                    "browser_screenshot: sidecar response missing `png_b64`".into(),
                )
            })?;
        // Reject oversized payloads before allocating the decoded
        // buffer. Base64 expansion is 4/3, so the upper bound on
        // decoded length is `b64.len() * 3 / 4`.
        let upper_bound = png_b64.len().saturating_mul(3) / 4;
        if upper_bound > MAX_SCREENSHOT_BYTES {
            return Err(ToolError::Execution(format!(
                "browser_screenshot: payload too large ({upper_bound} > {MAX_SCREENSHOT_BYTES} bytes); \
                 retry without full_page or capture a smaller region",
            )));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(png_b64.as_bytes())
            .map_err(|e| ToolError::Execution(format!("decode screenshot bytes: {e}")))?;
        if bytes.len() > MAX_SCREENSHOT_BYTES {
            return Err(ToolError::Execution(format!(
                "browser_screenshot: payload too large ({} > {MAX_SCREENSHOT_BYTES} bytes)",
                bytes.len()
            )));
        }
        let blob_ref = self
            .blob_store
            .put(&bytes, "image/png", None)
            .await
            .map_err(|e| ToolError::Execution(format!("store screenshot blob: {e}")))?;

        let url = result
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown url)");
        let text = format!(
            "Screenshot of {url} captured ({} bytes, blob_id={}).",
            bytes.len(),
            blob_ref.blob_id
        );

        Ok(ToolOutput::multi_modal_text(
            text,
            vec![aura_model::ContentBlock::Image {
                blob: blob_ref,
                mime_type: "image/png".to_string(),
            }],
        ))
    }
}
