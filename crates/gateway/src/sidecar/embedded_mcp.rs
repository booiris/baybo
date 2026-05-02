//! Gateway-side glue for embedded MCP servers.
//!
//! The protocol-policy types (`EmbeddedMcpProfile`, `browser_mcp_profile`,
//! `embedded_servers`) live in [`aura_tools::mcp::profile`]. This
//! module owns the host-tool integration: where to find the host's
//! `node` binary, which materialised bundle path each profile gets,
//! and the per-domain composition that turns the operator's
//! [`AuraConfig`] into the profile list the reconciler consumes.

use std::path::PathBuf;

use aura_config::AuraConfig;
use aura_tools::mcp::{EmbeddedMcpProfile, browser_mcp_profile};
use aura_workspace::WorkspacePaths;

use crate::sidecar::SidecarRuntime;

pub const NODE_BINARY_ENV: &str = "AURA_NODE_BIN";

/// Resolve the node binary used to spawn embedded MCP-server children.
/// Override with `AURA_NODE_BIN`; defaults to `node` on `PATH`.
///
/// `AURA_NODE_BIN` is intentionally still env-driven: it's a host-tool
/// pointer (where to find a `node` binary), not a runtime policy knob.
/// Mirrors `AURA_BUN_BIN` for channel sidecars.
pub fn node_binary() -> PathBuf {
    std::env::var_os(NODE_BINARY_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("node"))
}

/// Walk every tool-domain family and collect the [`EmbeddedMcpProfile`]
/// list to hand to [`aura_tools::mcp::embedded_servers`].
///
/// Each family entry below resolves its bundle path via
/// [`SidecarRuntime::bundle_for`] and gates on its config block (e.g.
/// `browser.enable=false` returns `None` and drops out of the list).
/// When no families fire, the returned vec is empty and the reconciler
/// just runs with the user-configured `.mcp.json` entries.
///
/// `workspace_paths` lets per-domain composition reach into the
/// workspace layout — currently used to pin `<work>/.fonts` as a Chrome
/// fontconfig search dir so user-dropped fonts (notably CJK) render in
/// screenshots without operator intervention.
///
/// Adding a future tool-domain MCP server (code_exec, db_query, …) is
/// one more entry in the array literal — `runtime::build_managers`
/// stays unchanged.
pub fn collect_profiles(
    runtime: &SidecarRuntime,
    config: &AuraConfig,
    workspace_paths: &WorkspacePaths,
) -> Vec<EmbeddedMcpProfile> {
    let node_cmd = node_binary().display().to_string();
    let browser_font_dir = workspace_paths.browser_fonts_dir();
    [runtime.bundle_for("browser").and_then(|bundle| {
        browser_mcp_profile(
            config.browser.enable,
            config.browser.chrome_path.as_deref(),
            config.browser.profile_dir.as_deref(),
            config.browser.sandbox,
            config.browser.width,
            config.browser.height,
            node_cmd.clone(),
            bundle,
            &[browser_font_dir.as_path()],
        )
    })]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_config::AuraConfig;

    fn ws() -> WorkspacePaths {
        WorkspacePaths::new(PathBuf::from("/tmp/aura-test-workspace"))
    }

    /// Disabled-by-default browser config produces zero embedded
    /// profiles even when the bundle is materialised. This is the
    /// boot-path equivalent of the `browser_mcp_profile` unit test —
    /// catches a typo on `config.browser.enable` that would silently
    /// bypass the gate.
    #[test]
    fn default_config_produces_no_profiles() {
        let Ok(rt) = SidecarRuntime::install() else {
            // Sidecar runtime unavailable in this build; the gate
            // would also produce zero profiles, just for a different
            // reason. Skip rather than fail spuriously.
            return;
        };
        let cfg = AuraConfig::default();
        assert!(!cfg.browser.enable, "default browser config is opt-in");
        assert!(
            collect_profiles(&rt, &cfg, &ws()).is_empty(),
            "browser.enable=false must keep the profile list empty even when the bundle is embedded",
        );
    }

    /// Flipping just `enable` produces exactly one profile and the
    /// `enable` flag actually flows to `browser_mcp_profile`. Together
    /// with the previous test this brackets the wiring of
    /// `config.browser.enable`.
    #[test]
    fn enabled_config_produces_browser_profile() {
        let Ok(rt) = SidecarRuntime::install() else {
            return;
        };
        if rt.bundle_for("browser").is_none() {
            return;
        }
        let mut cfg = AuraConfig::default();
        cfg.browser.enable = true;
        let profiles = collect_profiles(&rt, &cfg, &ws());
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].server_name, "browser");
        let font_dirs = profiles[0]
            .extra_env
            .get("AURA_BROWSER_EXTRA_FONT_DIRS")
            .expect("collect_profiles must pin <work>/.fonts as a Chrome fontconfig dir");
        assert_eq!(font_dirs, "/tmp/aura-test-workspace/work/.fonts");
    }
}
