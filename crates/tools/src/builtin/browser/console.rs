use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object, truncate_label};
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const EXPR_LABEL_MAX: usize = 80;

#[derive(Debug, Deserialize, Default)]
struct Params {
    #[serde(default)]
    clear: bool,
    #[serde(default)]
    expression: Option<String>,
}

pub struct BrowserConsoleTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserConsoleTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserConsoleTool {
    fn name(&self) -> &str {
        "browser_console"
    }

    fn description(&self) -> &str {
        "Inspect the page's console buffer and (optionally) evaluate a JavaScript \
         expression in the page context.\n\n\
         Without `expression`, returns recent console logs and uncaught errors. \
         With `expression` set, executes the JS and returns the value alongside any \
         logs produced. With `clear=true`, drops the buffered logs after returning \
         the current state.\n\n\
         JS evaluation is privileged (can read cookies, mutate the DOM, exfiltrate \
         page content) and prompts for explicit approval before each call."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "expression": {
                    "type": "string",
                    "description": "Optional JavaScript expression to evaluate in the page context"
                },
                "clear": {
                    "type": "boolean",
                    "description": "If true, clear the console buffer after returning its current state"
                }
            }),
            &[],
        )
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        // Only prompt when JS evaluation is requested. Pure log read /
        // clear is read-only against the buffered state and doesn't
        // need a gate.
        let expr = params.get("expression").and_then(|v| v.as_str());
        match expr {
            Some(e) if !e.is_empty() => vec![ResourceAccess::ExecCommand {
                command: format!("browser_console: {}", truncate_label(e, EXPR_LABEL_MAX)),
            }],
            _ => Vec::new(),
        }
    }

    fn call_label(&self, params: &Value) -> Option<String> {
        params
            .get("expression")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| truncate_label(s, EXPR_LABEL_MAX))
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let result = call_sidecar(
            &self.client,
            "eval",
            json!({
                "clear": p.clear,
                "expression": p.expression,
            }),
            ctx,
        )
        .await?;
        Ok(ToolOutput::Json(result))
    }
}
