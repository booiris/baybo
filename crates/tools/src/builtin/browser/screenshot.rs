use std::sync::Arc;

use async_trait::async_trait;
use aura_storage::BlobStore;
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object};
use crate::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, Default)]
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

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParams(e.to_string()))
            .unwrap_or_default();
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
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(png_b64.as_bytes())
            .map_err(|e| ToolError::Execution(format!("decode screenshot bytes: {e}")))?;
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
