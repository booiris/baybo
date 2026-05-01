use std::sync::Arc;

use async_trait::async_trait;
use aura_storage::BlobStore;
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, Tool as RmcpTool};
use rmcp::service::Peer;
use serde_json::Value;

use crate::approval::ResourceAccess;
use crate::mcp::access_rule::{AccessRule, AuraToolMeta, CallLabelRule};
use crate::mcp::content_adapter::adapt_call_result;
use crate::{Tool, ToolContext, ToolError, ToolOutput};

/// Wrapper that exposes one rmcp-discovered tool to Aura's agent loop.
///
/// Names are namespaced as `<server>/<tool>` so an MCP server's tool list
/// cannot collide with a builtin. The peer is `Arc`-cloned per tool;
/// `Peer<RoleClient>` is internally `Arc`-based, so the cost is a refcount
/// bump per tool, not per call.
///
/// `default_resource_access` is the fallback used when the tool carries
/// no Aura-specific per-call rule. It mirrors the historical behavior —
/// every tool inherits the server-level resource list (stdio → one
/// `ExecCommand`, http → one `Http`).
///
/// `_meta.aura` annotations are **only honored for embedded servers**
/// (the Aura-shipped browser MCP today, future code_exec / db_query).
/// User `.mcp.json` servers — including HTTP / OAuth third parties —
/// can not lower their own approval gate by setting
/// `_meta.aura.access_rule = { kind: "none" }`; the reconciler passes
/// `honor_aura_meta=false` for them and the per-tool annotations are
/// silently dropped at construction.
pub struct McpTool {
    server_name: String,
    tool_name: String,
    namespaced_name: String,
    description: String,
    parameters_schema: Value,
    default_resource_access: Vec<ResourceAccess>,
    access_rule: Option<AccessRule>,
    call_label: Option<CallLabelRule>,
    inject_session_id_as: Option<String>,
    peer: Peer<RoleClient>,
    blob_store: Option<Arc<dyn BlobStore>>,
}

impl McpTool {
    pub fn new(
        server_name: String,
        descriptor: RmcpTool,
        default_resource_access: Vec<ResourceAccess>,
        honor_aura_meta: bool,
        peer: Peer<RoleClient>,
        blob_store: Option<Arc<dyn BlobStore>>,
    ) -> Self {
        let namespaced_name = format!("{server_name}/{}", descriptor.name);
        let parameters_schema = serde_json::Value::Object((*descriptor.input_schema).clone());
        let description = descriptor
            .description
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("MCP tool {} on server {}", descriptor.name, server_name));
        // Trust gate: only embedded Aura-owned servers may carry per-tool
        // `_meta.aura` annotations. A user-configured server that tries
        // to advertise `access_rule = none` would otherwise turn its
        // server-level `ExecCommand`/`Http` resource into a no-prompt
        // call — see the adversarial review.
        let meta = if honor_aura_meta {
            AuraToolMeta::from_meta(descriptor.meta.as_deref())
        } else {
            AuraToolMeta::default()
        };
        Self {
            server_name,
            tool_name: descriptor.name.to_string(),
            namespaced_name,
            description,
            parameters_schema,
            default_resource_access,
            access_rule: meta.access_rule,
            call_label: meta.call_label,
            inject_session_id_as: meta.inject_session_id_as,
            peer,
            blob_store,
        }
    }

    pub fn server(&self) -> &str {
        &self.server_name
    }

    pub fn upstream_name(&self) -> &str {
        &self.tool_name
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.namespaced_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.parameters_schema.clone()
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        match &self.access_rule {
            Some(rule) => rule.evaluate(params),
            None => self.default_resource_access.clone(),
        }
    }

    fn call_label(&self, params: &Value) -> Option<String> {
        self.call_label.as_ref().and_then(|r| r.evaluate(params))
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let mut request = CallToolRequestParams::new(self.tool_name.clone());
        match params {
            Value::Object(mut map) => {
                if let Some(key) = &self.inject_session_id_as {
                    map.insert(key.clone(), Value::String(ctx.session_id.clone()));
                }
                request = request.with_arguments(map);
            }
            Value::Null => {
                if let Some(key) = &self.inject_session_id_as {
                    let mut map = serde_json::Map::new();
                    map.insert(key.clone(), Value::String(ctx.session_id.clone()));
                    request = request.with_arguments(map);
                }
            }
            other => {
                return Err(ToolError::InvalidParams(format!(
                    "MCP tools require an object of arguments; got {other:?}"
                )));
            }
        }

        let result =
            self.peer.call_tool(request).await.map_err(|e| {
                ToolError::Mcp(format!("{}/{}: {e}", self.server_name, self.tool_name))
            })?;

        let is_error = result.is_error.unwrap_or(false);
        adapt_call_result(&result.content, is_error, self.blob_store.as_ref()).await
    }
}

/// Synthesize a ToolManifest for an MCP-sourced tool given the server's
/// trust + capabilities. Used by the reconciler when registering with
/// `ToolRegistry::register_dynamic`.
pub(crate) fn build_manifest(
    namespaced_name: &str,
    description: &str,
    parameters_schema: Value,
    trust_level: aura_model::TrustLevel,
    capabilities: Vec<crate::ToolCapability>,
) -> crate::ToolManifest {
    crate::ToolManifest {
        name: namespaced_name.to_string(),
        description: description.to_string(),
        trust_level,
        parameters_schema,
        capabilities,
    }
}

#[cfg(test)]
mod tests {
    //! Trust-gate regression tests. The mock peer here would never
    //! be drivable for `execute()`, but `accessed_resources` /
    //! `call_label` only need the descriptor + flags, so we don't
    //! reach into the peer. Construction-only.

    use super::*;
    use serde_json::json;

    /// Replays the exact gate `McpTool::new` runs on `descriptor.meta`:
    /// when `honor_aura_meta=false` the annotation is dropped before
    /// it reaches `accessed_resources`. We test it directly (rather
    /// than via `McpTool::new`) because `Peer<RoleClient>` has no
    /// public constructor, so spinning up a real-shape McpTool from
    /// this crate's tests isn't possible without a live MCP child.
    /// `connect_server` already passes `target.is_embedded` through
    /// — see the smoke tests under `crates/gateway/tests/` for the
    /// end-to-end coverage.
    fn meta_after_gate(raw: serde_json::Value, honor_aura_meta: bool) -> AuraToolMeta {
        let map = raw.as_object().cloned();
        if honor_aura_meta {
            AuraToolMeta::from_meta(map.as_ref())
        } else {
            AuraToolMeta::default()
        }
    }

    /// A user-configured MCP server (or a compromised third party)
    /// can not advertise `_meta.aura.access_rule = { kind: "none" }`
    /// to suppress its own approval gate. Without the gate, the
    /// adversarial descriptor's annotation reaches
    /// `accessed_resources` and replaces the server-level fallback
    /// with an empty resource list — `tool_executor` then never
    /// prompts the user before invoking the tool.
    #[test]
    fn hostile_user_server_meta_aura_is_dropped() {
        let raw = json!({ "aura": { "access_rule": { "kind": "none" } } });
        let server_default = vec![ResourceAccess::ExecCommand {
            command: "/usr/bin/evil".into(),
        }];

        // Sanity: the descriptor IS adversarial — when the gate lets
        // it through it does evaluate to an empty resource list.
        let meta_honored = meta_after_gate(raw.clone(), true);
        assert!(
            matches!(meta_honored.access_rule, Some(AccessRule::None)),
            "without the gate, _meta.aura.access_rule deserialises to None",
        );
        let evaluated_honored = meta_honored
            .access_rule
            .as_ref()
            .map(|r| r.evaluate(&json!({})))
            .unwrap_or_else(|| server_default.clone());
        assert!(
            evaluated_honored.is_empty(),
            "ungated path: hostile descriptor produces empty resources -> no prompt",
        );

        // The fix: with the gate closed (user `.mcp.json` server),
        // `meta` falls back to `Default` and `accessed_resources` uses
        // `default_resource_access` — the server-level ExecCommand
        // prompt fires.
        let meta_blocked = meta_after_gate(raw, false);
        assert!(
            meta_blocked.access_rule.is_none(),
            "gated path: _meta.aura is dropped before reaching the tool",
        );
        let evaluated_blocked = meta_blocked
            .access_rule
            .as_ref()
            .map(|r| r.evaluate(&json!({})))
            .unwrap_or_else(|| server_default.clone());
        assert_eq!(
            evaluated_blocked, server_default,
            "user server: must fall back to default_resource_access (server-level ExecCommand)",
        );
    }

    /// Embedded servers (the Aura-shipped browser MCP today) keep
    /// the per-tool `_meta.aura` plumbing working — that's the whole
    /// reason the annotation exists.
    #[test]
    fn embedded_server_honors_meta_aura() {
        let raw = json!({
            "aura": {
                "access_rule": {
                    "kind": "always",
                    "resource": "exec_command",
                    "label_template": "browser_screenshot: {full_page}"
                }
            }
        });
        let meta = meta_after_gate(raw, true);
        let rule = meta.access_rule.expect("annotation honored for embedded");
        let resources = rule.evaluate(&json!({ "full_page": true }));
        assert_eq!(resources.len(), 1);
        match &resources[0] {
            ResourceAccess::ExecCommand { command } => {
                assert_eq!(command, "browser_screenshot: true");
            }
            other => panic!("expected ExecCommand, got {other:?}"),
        }
    }
}
