use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object, truncate_label};
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const METHOD_LABEL_MAX: usize = 80;
/// Length of the hex digest folded into every approval-cache key.
/// 12 hex chars (48 bits) is plenty to make the chance of an
/// accidental collision between two distinct CDP calls negligible.
const PARAM_DIGEST_LEN: usize = 12;

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
        // Raw CDP can do anything: read cookies, intercept network,
        // evaluate JS in any frame, kill the browser. The approval
        // gate caches under the exact `command` string, so a naive
        // "browser_cdp: {method}" key would let a single
        // "approve always" decision cover *every* future call to
        // that method — including completely different `params` /
        // `target_id` / `frame_id`. Fold a stable digest of those
        // discriminating fields into the key so each distinct call
        // is a distinct approval.
        let method = params
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("(missing method)");
        vec![ResourceAccess::ExecCommand {
            command: format!(
                "browser_cdp: {} #{}",
                truncate_label(method, METHOD_LABEL_MAX),
                cache_discriminator(params)
            ),
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

/// Stable hex digest of the call's discriminating fields (`params`,
/// `target_id`, `frame_id`). Folded into the approval-cache key so
/// "approve always" applies only to the exact call shape, not to
/// every future call with the same `method`. JSON canonicalisation
/// is good-enough for our purposes: the LLM emits the same object
/// each time for the same logical call, and a false cache miss is
/// safer than a false cache hit.
fn cache_discriminator(params: &Value) -> String {
    let mut hasher = Sha256::new();
    fn feed(hasher: &mut Sha256, params: &Value, key: &str) {
        if let Some(v) = params.get(key) {
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(v.to_string().as_bytes());
            hasher.update(b";");
        }
    }
    feed(&mut hasher, params, "params");
    feed(&mut hasher, params, "target_id");
    feed(&mut hasher, params, "frame_id");
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(PARAM_DIGEST_LEN);
    for byte in digest.iter().take(PARAM_DIGEST_LEN.div_ceil(2)) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex.truncate(PARAM_DIGEST_LEN);
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_params_yield_distinct_discriminators() {
        let a = json!({ "method": "Runtime.evaluate", "params": { "expression": "1+1" } });
        let b = json!({ "method": "Runtime.evaluate", "params": { "expression": "2+2" } });
        assert_ne!(cache_discriminator(&a), cache_discriminator(&b));
    }

    #[test]
    fn same_params_yield_same_discriminator() {
        let a = json!({ "method": "Runtime.evaluate", "params": { "expression": "1+1" } });
        let b = json!({ "method": "Runtime.evaluate", "params": { "expression": "1+1" } });
        assert_eq!(cache_discriminator(&a), cache_discriminator(&b));
    }

    #[test]
    fn target_id_changes_discriminator() {
        let a = json!({ "method": "Page.reload", "target_id": "T-1" });
        let b = json!({ "method": "Page.reload", "target_id": "T-2" });
        assert_ne!(cache_discriminator(&a), cache_discriminator(&b));
    }

    #[test]
    fn missing_params_still_produce_stable_digest() {
        let a = json!({ "method": "Browser.getVersion" });
        let b = json!({ "method": "Browser.getVersion" });
        assert_eq!(cache_discriminator(&a), cache_discriminator(&b));
        assert_eq!(cache_discriminator(&a).len(), PARAM_DIGEST_LEN);
    }

    #[test]
    fn approval_command_includes_discriminator() {
        let tool = BrowserCdpTool::new(Arc::new(crate::builtin::browser::tests::DeadClient));
        let access = tool.accessed_resources(&json!({
            "method": "Runtime.evaluate",
            "params": { "expression": "alert(1)" }
        }));
        let ResourceAccess::ExecCommand { command } = &access[0] else {
            panic!("expected ExecCommand");
        };
        assert!(command.contains("Runtime.evaluate"));
        assert!(
            command.contains('#'),
            "command must include hash discriminator: {command}"
        );
    }
}
