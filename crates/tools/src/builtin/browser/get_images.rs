use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object};
use crate::{Tool, ToolContext, ToolOutput};

pub struct BrowserGetImagesTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserGetImagesTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserGetImagesTool {
    fn name(&self) -> &str {
        "browser_get_images"
    }

    fn description(&self) -> &str {
        "Enumerate all `<img>` elements on the current page (excluding inline data: \
         URIs). Returns an array of `{ src, alt, w, h }` objects so the agent can \
         pick which to load via `WebFetch` or describe via `browser_screenshot`."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(json!({}), &[])
    }

    async fn execute(&self, _params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let result = call_sidecar(&self.client, "get_images", json!({}), ctx).await?;
        Ok(ToolOutput::Json(result))
    }
}
