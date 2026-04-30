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
    direction: Direction,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Direction {
    Up,
    Down,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
        }
    }
}

pub struct BrowserScrollTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserScrollTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserScrollTool {
    fn name(&self) -> &str {
        "browser_scroll"
    }

    fn description(&self) -> &str {
        "Scroll the viewport ~500 pixels in the given direction (`up` or `down`)."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "direction": {
                    "type": "string",
                    "enum": ["up", "down"],
                    "description": "Scroll direction"
                }
            }),
            &["direction"],
        )
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let result = call_sidecar(
            &self.client,
            "scroll",
            json!({ "direction": p.direction.as_str() }),
            ctx,
        )
        .await?;
        Ok(ToolOutput::Json(result))
    }
}
