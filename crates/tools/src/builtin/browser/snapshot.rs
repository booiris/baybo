use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object};
use crate::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize, Default)]
struct Params {
    #[serde(default)]
    full: bool,
}

pub struct BrowserSnapshotTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserSnapshotTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserSnapshotTool {
    fn name(&self) -> &str {
        "browser_snapshot"
    }

    fn description(&self) -> &str {
        "Return the current page's accessibility-tree snapshot (Playwright's \
         ariaSnapshot, with `@eN` element refs the click / type / press tools \
         take). Includes a supervisor block listing pending JS dialogs, a \
         bounded frame tree, and recent console errors. `full=true` returns \
         the unbounded snapshot; default is bounded for token efficiency."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "full": {
                    "type": "boolean",
                    "description": "If true, return the full snapshot without bounding (default false)"
                }
            }),
            &[],
        )
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParams(e.to_string()))
            .unwrap_or_default();
        let result = call_sidecar(&self.client, "snapshot", json!({ "full": p.full }), ctx).await?;
        // The sidecar returns either { "text": "..." } (preferred) or a
        // raw string. Surface as Text so the LLM sees plain markup.
        let text = result
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
            });
        Ok(ToolOutput::Text(text))
    }
}
