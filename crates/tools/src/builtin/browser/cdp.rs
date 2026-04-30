use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object, truncate_label};
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const METHOD_LABEL_MAX: usize = 80;

#[derive(Debug, Deserialize)]
struct Params {
    method: String,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    frame_id: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct BrowserCdpTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserCdpTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserCdpTool {
    fn name(&self) -> &str {
        "browser_cdp"
    }

    fn description(&self) -> &str {
        "Send a raw Chrome DevTools Protocol command to the underlying browser. \
         Use this for capabilities the higher-level browser_* tools don't cover \
         (cookies via `Network.getAllCookies`, network interception via \
         `Fetch.*`, low-level page control, tab enumeration, …).\n\n\
         Every call is privileged and prompts for approval. The `params` argument \
         is forwarded verbatim. `target_id` / `frame_id` route the call to a \
         specific tab or out-of-process iframe; omit for top-level page."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "method": {
                    "type": "string",
                    "description": "CDP method name, e.g. `Network.getAllCookies`"
                },
                "params": {
                    "type": "object",
                    "description": "Method-specific parameters (forwarded verbatim)"
                },
                "target_id": {
                    "type": "string",
                    "description": "Optional CDP target id for tab-level methods"
                },
                "frame_id": {
                    "type": "string",
                    "description": "Optional out-of-process iframe id for cross-origin scope"
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional per-call timeout (ms); falls back to the tool timeout"
                }
            }),
            &["method"],
        )
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        // Always prompt. Raw CDP can do anything: read cookies,
        // intercept network, evaluate JS in any frame, kill the
        // browser. The method name itself is the trust boundary, so
        // surface it through the gate verbatim.
        let method = params
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("(missing method)");
        vec![ResourceAccess::ExecCommand {
            command: format!("browser_cdp: {}", truncate_label(method, METHOD_LABEL_MAX)),
        }]
    }

    fn call_label(&self, params: &Value) -> Option<String> {
        params
            .get("method")
            .and_then(|v| v.as_str())
            .map(|s| truncate_label(s, METHOD_LABEL_MAX))
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let result = call_sidecar(
            &self.client,
            "cdp",
            json!({
                "method": p.method,
                "params": p.params,
                "target_id": p.target_id,
                "frame_id": p.frame_id,
                "timeout_ms": p.timeout_ms,
            }),
            ctx,
        )
        .await?;
        Ok(ToolOutput::Json(result))
    }
}
