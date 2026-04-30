use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object};
use crate::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize)]
struct Params {
    r#ref: String,
}

pub struct BrowserClickTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserClickTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserClickTool {
    fn name(&self) -> &str {
        "browser_click"
    }

    fn description(&self) -> &str {
        "Click an interactive element identified by a snapshot `@eN` ref. \
         Use `browser_snapshot` first to see element refs."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "ref": {
                    "type": "string",
                    "description": "Element reference from a prior snapshot, e.g. `@e5`"
                }
            }),
            &["ref"],
        )
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let result = call_sidecar(&self.client, "click", json!({ "ref": p.r#ref }), ctx).await?;
        Ok(ToolOutput::Json(result))
    }
}
