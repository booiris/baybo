use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::builtin::browser::client::BrowserSidecarClient;
use crate::builtin::browser::schema::{call_sidecar, schema_object, truncate_label};
use crate::builtin::url_policy::{host_to_literal_ip, validate_url_with};
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const CALL_LABEL_MAX: usize = 120;

#[derive(Debug, Deserialize)]
struct Params {
    url: String,
}

pub struct BrowserNavigateTool {
    client: Arc<dyn BrowserSidecarClient>,
}

impl BrowserNavigateTool {
    pub fn new(client: Arc<dyn BrowserSidecarClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for BrowserNavigateTool {
    fn name(&self) -> &str {
        "browser_navigate"
    }

    fn description(&self) -> &str {
        "Open a URL in the agent's isolated browser context (Playwright + bundled \
         Chromium, dedicated profile dir, no shared cookies with the user's normal \
         browser). Returns the page title, final URL after redirects, and a compact \
         accessibility-tree snapshot with `@eN` element references the other \
         `browser_*` tools (click, type, …) take.\n\n\
         Same SSRF floor as `WebFetch`: only `http(s)`, literal-IP hosts in \
         loopback / RFC1918 / link-local / metadata / unique-local ranges are \
         rejected. The browser context is created lazily and persists across calls \
         in the same session until idle GC reaps it (10 min)."
    }

    fn parameters_schema(&self) -> Value {
        schema_object(
            json!({
                "url": { "type": "string", "format": "uri", "description": "The http(s) URL to open" }
            }),
            &["url"],
        )
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        // Same three-bucket rule as WebFetch: hostnames bypass (DNS
        // resolver inside the sidecar enforces SSRF), blocked literal
        // IPs bypass (validate_url_with rejects them pre-RPC), only
        // literal *public* IPs prompt. Strict policy — sidecar always
        // runs with `allow_loopback = false`.
        params
            .get("url")
            .and_then(|v| v.as_str())
            .and_then(|s| url::Url::parse(s).ok())
            .and_then(|u| u.host_str().map(str::to_lowercase))
            .filter(|host| {
                let Some(addr) = host_to_literal_ip(host) else {
                    return false;
                };
                !aura_security::is_blocked_ip(&addr, false)
            })
            .map(|host| vec![ResourceAccess::Http { host }])
            .unwrap_or_default()
    }

    fn call_label(&self, params: &Value) -> Option<String> {
        params
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| truncate_label(s, CALL_LABEL_MAX))
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        validate_url_with(&p.url, false)
            .map_err(|e| ToolError::InvalidParams(format!("browser_navigate: {e}")))?;
        let result = call_sidecar(&self.client, "navigate", json!({ "url": p.url }), ctx).await?;
        Ok(ToolOutput::Json(result))
    }
}
