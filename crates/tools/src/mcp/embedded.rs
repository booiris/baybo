//! Always-on MCP servers shipped inside the gateway binary.
//!
//! User-configured MCP servers come from the workspace's `.mcp.json`
//! file. Embedded servers (currently: the browser sidecar) are baked
//! into the binary and shouldn't appear in or be editable via that
//! file. The reconciler accepts a separate stream of [`EmbeddedMcpServer`]
//! entries at construction time and merges them with the user list on
//! every tick.
//!
//! The struct is consumed by [`crate::mcp::McpReconciler::new`]; the
//! gateway builds it from `aura_gateway::sidecar::SidecarRuntime` (which
//! lives in `aura-gateway` and isn't visible from this crate).

use std::collections::HashMap;

use crate::mcp::config::McpServerEntry;

/// One always-on MCP server, plus boot-config env vars merged onto its
/// stdio child after the secret-vault env loader runs. Vault entries
/// keep their precedence so an operator who really wants to override a
/// boot-config value can stash it under `mcp.<name>.env`.
#[derive(Debug, Clone)]
pub struct EmbeddedMcpServer {
    pub entry: McpServerEntry,
    pub extra_env: HashMap<String, String>,
}

impl EmbeddedMcpServer {
    pub(crate) fn new(entry: McpServerEntry, extra_env: HashMap<String, String>) -> Self {
        Self { entry, extra_env }
    }
}
