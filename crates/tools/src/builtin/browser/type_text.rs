use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object};
use crate::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    r#ref: String,
    text: String,
}

pub struct BrowserTypeTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserTypeTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserTypeTool {
    fn name(&self) -> &str {
        "browser_type"
    }

    fn description(&self) -> &str {
        "Clear an input element identified by `@eN` and type the given text into it. \
         Use `browser_press` afterwards for keys like Enter / Tab."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "ref": { "type": "string", "description": "Element reference from a prior snapshot" },
                "text": { "type": "string", "description": "Text to type into the input" }
            }),
            &["ref", "text"],
        )
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let result = call_sidecar(
            &self.client,
            "type",
            json!({ "ref": p.r#ref, "text": p.text }),
            ctx,
        )
        .await?;
        Ok(ToolOutput::Json(result))
    }
}
