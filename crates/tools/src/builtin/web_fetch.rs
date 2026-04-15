//! `WebFetch` — fetch a URL and return its body.
//!
//! The Claude Code spec also runs the body through a small LLM to extract
//! just the bit the caller asked about; Aura doesn't have a side-channel LLM
//! here yet, so for now we return the (truncated) raw body and let the
//! caller's main-loop model do the extraction.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolError, ToolOutput};

const MAX_BODY_BYTES: usize = 256 * 1024;

pub struct WebFetchTool;

#[derive(Debug, Deserialize)]
struct Params {
    url: String,
    #[serde(default)]
    #[allow(dead_code)]
    prompt: Option<String>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return its body (text or JSON). HTTP is upgraded to \
         HTTPS. Large responses are truncated to 256 KiB. The `prompt` field \
         is accepted for API parity with Claude Code's WebFetch but is not \
         currently used for extraction."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url":    { "type": "string", "description": "Fully-formed URL to fetch" },
                "prompt": { "type": "string", "description": "Optional extraction hint (not yet used)" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let url = if let Some(stripped) = p.url.strip_prefix("http://") {
            format!("https://{stripped}")
        } else {
            p.url
        };

        let client = reqwest::Client::builder()
            .timeout(ctx.timeout)
            .build()
            .map_err(|e| ToolError::Execution(format!("build client: {e}")))?;

        let fetch = async {
            let resp = client
                .get(&url)
                .send()
                .await
                .map_err(|e| ToolError::Execution(format!("GET {url}: {e}")))?;
            let status = resp.status();
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| ToolError::Execution(format!("read body: {e}")))?;
            Ok::<_, ToolError>((status, content_type, bytes))
        };

        let (status, content_type, bytes) = tokio::select! {
            _ = ctx.cancellation_token.cancelled() => {
                return Err(ToolError::Execution("cancelled".into()));
            }
            res = fetch => res?,
        };

        let truncated = bytes.len() > MAX_BODY_BYTES;
        let slice = &bytes[..bytes.len().min(MAX_BODY_BYTES)];
        let mut body = String::from_utf8_lossy(slice).into_owned();
        if truncated {
            body.push_str("\n… [truncated]");
        }

        Ok(ToolOutput::Json(json!({
            "url": url,
            "status": status.as_u16(),
            "content_type": content_type,
            "body": body,
        })))
    }
}
