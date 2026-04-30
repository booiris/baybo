use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object};
use crate::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize)]
struct Params {
    key: String,
}

pub struct BrowserPressTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserPressTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserPressTool {
    fn name(&self) -> &str {
        "browser_press"
    }

    fn description(&self) -> &str {
        "Press a single keyboard key in the page's currently-focused element. \
         Examples: `Enter`, `Tab`, `Escape`, `ArrowDown`, `Control+A`."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "key": {
                    "type": "string",
                    "description": "Playwright key spec (e.g. `Enter`, `Control+A`)"
                }
            }),
            &["key"],
        )
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let result = call_sidecar(&self.client, "press", json!({ "key": p.key }), ctx).await?;
        Ok(ToolOutput::Json(result))
    }
}
