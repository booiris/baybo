//! Shared helpers for the browser-tool family. Each `*Tool` struct
//! holds an `Arc<dyn BrowserSidecarClient>`, deserialises its own
//! `Params`, and forwards to the sidecar via [`call_sidecar`].

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::builtin::browser::client::{BrowserRpcError, BrowserSidecarClient};
use crate::{ToolContext, ToolError};

/// Forward an RPC to the sidecar with the tool's timeout +
/// cancellation token already plumbed through. Tools do not need to
/// build a `tokio::select!` themselves — the sidecar client trait
/// owns timeout/cancellation semantics.
pub(crate) async fn call_sidecar<P: Serialize>(
    client: &Arc<dyn BrowserSidecarClient>,
    method: &str,
    params: P,
    ctx: &ToolContext,
) -> Result<Value, ToolError> {
    let raw = serde_json::to_value(params)
        .map_err(|e| ToolError::InvalidParams(format!("encode {method} params: {e}")))?;
    let token = clone_cancel(&ctx.cancellation_token);
    client
        .call(method, raw, &ctx.session_id, ctx.timeout, token)
        .await
        .map_err(ToolError::from)
}

fn clone_cancel(t: &CancellationToken) -> CancellationToken {
    t.clone()
}

/// Build a `BrowserRpcError::Sidecar` with a stable error code; used
/// by the trait's concrete impl when the sidecar returns
/// `{ "error": { code, message } }`.
#[allow(dead_code)]
pub(crate) fn sidecar_error(
    code: impl Into<String>,
    message: impl Into<String>,
) -> BrowserRpcError {
    BrowserRpcError::Sidecar {
        code: code.into(),
        message: message.into(),
    }
}

/// Truncate a string to at most `max` characters at a UTF-8 boundary,
/// appending `…` when truncation actually occurred. Used by tools
/// that surface user-supplied strings (URL, JS expression, CDP method)
/// in approval-prompt labels.
pub(crate) fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Boilerplate-free `parameters_schema` builder. Most browser tools
/// have a tiny schema shape `{ type: object, properties: {...},
/// required: [...] }`; this just composes the JSON.
pub(crate) fn schema_object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}
