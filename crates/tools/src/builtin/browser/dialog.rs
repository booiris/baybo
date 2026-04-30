use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object};
use crate::{Tool, ToolContext, ToolError, ToolOutput};

#[derive(Debug, Deserialize)]
struct Params {
    action: String,
    #[serde(default)]
    prompt_text: Option<String>,
    #[serde(default)]
    dialog_id: Option<String>,
}

pub struct BrowserDialogTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserDialogTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserDialogTool {
    fn name(&self) -> &str {
        "browser_dialog"
    }

    fn description(&self) -> &str {
        "Respond to a native JavaScript dialog (`alert` / `confirm` / `prompt` / \
         `beforeunload`) currently blocking the page. Use `browser_snapshot` first \
         to inspect the supervisor block listing pending dialogs.\n\n\
         `action`: `accept` or `dismiss`. For `prompt` dialogs accepted with a \
         response, supply `prompt_text`. When multiple dialogs are queued, use \
         `dialog_id` (from the snapshot) to disambiguate; otherwise the head of \
         the queue is responded to."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "action": {
                    "type": "string",
                    "enum": ["accept", "dismiss"],
                    "description": "Whether to accept or dismiss the dialog"
                },
                "prompt_text": {
                    "type": "string",
                    "description": "Response text for `prompt` dialogs accepted with a value"
                },
                "dialog_id": {
                    "type": "string",
                    "description": "Specific pending dialog to respond to (defaults to head of queue)"
                }
            }),
            &["action"],
        )
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        if p.action != "accept" && p.action != "dismiss" {
            return Err(ToolError::InvalidParams(format!(
                "browser_dialog: action must be `accept` or `dismiss`, got `{}`",
                p.action
            )));
        }
        let result = call_sidecar(
            &self.client,
            "dialog",
            json!({
                "action": p.action,
                "prompt_text": p.prompt_text,
                "dialog_id": p.dialog_id,
            }),
            ctx,
        )
        .await?;
        Ok(ToolOutput::Json(result))
    }
}
