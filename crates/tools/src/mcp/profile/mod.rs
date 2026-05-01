//! Embedded MCP-server profiles.
//!
//! A profile describes one always-on MCP server that ships inside the
//! Aura binary: the stdio command + args to spawn, the MCP-side server
//! name (the `<server>/<tool>` namespace the LLM sees), the capability
//! ceiling, and any boot-config env vars to merge into the child.
//!
//! Each tool-domain family (browser today; future code_exec, db_query,
//! …) ships a `*_mcp_profile()` builder under this module that consults
//! its own config block and returns either `Some(EmbeddedMcpProfile)`
//! (when enabled) or `None`.
//!
//! The boot path collects whichever profiles fire and hands the list to
//! [`embedded_servers`], which materialises each one into the
//! [`EmbeddedMcpServer`] entry the reconciler consumes.
//!
//! Why this lives in `aura-tools` and not in `aura-gateway`: the
//! profile is policy (how to translate config knobs into env vars,
//! which capabilities a family declares, which server name the LLM
//! sees). The gateway's role is purely OS plumbing — locating the
//! materialised bundle path on disk and resolving the host's `node`
//! binary — and it passes those two strings into the profile builders
//! defined here.

pub mod browser;

use std::collections::HashMap;

use crate::ToolCapability;
use crate::mcp::config::{McpServerEntry, McpTransportConfig, TrustLevelConfig};
use crate::mcp::embedded::EmbeddedMcpServer;

pub use browser::{BlobUploadEnv, browser_mcp_profile};

/// One embedded MCP server's wiring. Tool-domain families build these
/// via their own `*_mcp_profile()` helper from the corresponding config
/// block; the boot path collects them and hands the list to
/// [`embedded_servers`].
#[derive(Debug, Clone)]
pub struct EmbeddedMcpProfile {
    /// MCP server name as the agent loop sees it. Tools surface as
    /// `<server_name>/<tool>` to the LLM.
    pub server_name: String,
    /// Stdio command to spawn (typically the host's `node` binary).
    pub command: String,
    /// Args appended to `command` (typically the materialised bundle
    /// path).
    pub args: Vec<String>,
    /// Capability ceiling for the synthesised manifest. The per-tool
    /// `_meta.aura.access_rule` annotations refine this per call.
    pub capabilities: Vec<ToolCapability>,
    /// Boot-config env vars merged onto the child's env after the
    /// secret-vault load (so `mcp.<server_name>.env` vault entries
    /// retain precedence). Use this for non-secret runtime knobs the
    /// child reads via `process.env.*` (chromium binary path,
    /// no-sandbox flag, …).
    pub extra_env: HashMap<String, String>,
}

/// Materialise each profile into an [`EmbeddedMcpServer`] entry the
/// reconciler can spawn. Pure data shaping — no I/O, no SidecarRuntime
/// dependency. Callers (the gateway boot path) resolve bundle paths
/// and the host node binary up-front and bake them into each profile.
pub fn embedded_servers(profiles: &[EmbeddedMcpProfile]) -> Vec<EmbeddedMcpServer> {
    profiles
        .iter()
        .map(|p| {
            let entry = McpServerEntry {
                name: p.server_name.clone(),
                transport: McpTransportConfig::Stdio {
                    command: p.command.clone(),
                    args: p.args.clone(),
                },
                trust_level: TrustLevelConfig::Trusted,
                capabilities: p.capabilities.clone(),
                oauth: None,
            };
            EmbeddedMcpServer::new(entry, p.extra_env.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_profiles_yields_empty_entries() {
        assert!(embedded_servers(&[]).is_empty());
    }

    #[test]
    fn profile_round_trips_through_entry_shape() {
        let mut env = HashMap::new();
        env.insert("FOO".into(), "bar".into());
        let p = EmbeddedMcpProfile {
            server_name: "test".into(),
            command: "/usr/bin/node".into(),
            args: vec!["/path/to/bundle.mjs".into()],
            capabilities: vec![ToolCapability::Http],
            extra_env: env,
        };
        let entries = embedded_servers(std::slice::from_ref(&p));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry.name, "test");
        assert_eq!(entries[0].extra_env.get("FOO").unwrap(), "bar");
        match &entries[0].entry.transport {
            McpTransportConfig::Stdio { command, args } => {
                assert_eq!(command, "/usr/bin/node");
                assert_eq!(args, &vec!["/path/to/bundle.mjs".to_string()]);
            }
            _ => panic!("expected stdio transport"),
        }
    }
}
