//! Browser embedded MCP server: profile builder.
//!
//! The user-facing config struct lives in `aura_config::BrowserConfig`
//! (so `aura.json` deserialisation has no `aura-tools` runtime dep);
//! this module owns the *policy translation* — turning the operator's
//! settings into the concrete env vars + capability list the embedded
//! MCP child consumes.
//!
//! [`browser_mcp_profile`] takes primitive args rather than a typed
//! config struct so this crate stays independent of `aura-config`. The
//! gateway boot path does the trivial unpacking (`config.browser.enable`,
//! `config.browser.sandbox`, …).

use std::collections::HashMap;
use std::path::Path;

use super::EmbeddedMcpProfile;
use crate::ToolCapability;

/// Build the `browser` MCP-server profile from the operator's policy
/// settings. Returns `None` when `enable=false` so the boot path drops
/// it uniformly with the rest of the disabled profiles.
///
/// Args:
/// - `enable`: master switch — `false` returns `None`.
/// - `sandbox`: when `false`, sets `AURA_BROWSER_NO_SANDBOX=1` so the
///   TS sidecar launches Chromium with `--no-sandbox`.
/// - `chrome_path`, `profile_dir`: optional overrides; when set, land
///   in `AURA_CHROMIUM_BIN` / `AURA_BROWSER_PROFILE_DIR` env vars.
/// - `allow_loopback`: test-only escape hatch — admits 127.0.0.0/8 +
///   `::1` via `AURA_BROWSER_ALLOW_LOOPBACK=1`.
/// - `command`: typically the resolved host `node` binary
///   (`aura_gateway::node_binary().display().to_string()`).
/// - `bundle_path`: the materialised `dist/bundle.mjs` path
///   (`runtime.bundle_for("browser")`).
///
/// `blob_upload` lets large screenshots stream out via the gateway's
/// `/v1/blobs` endpoint instead of inlining 16 MiB of base64 in the
/// MCP frame. The TS sidecar reads `AURA_CHANNEL_PORT_FILE` lazily
/// on first upload to discover the gateway's loopback port (avoids
/// the boot-order chicken-and-egg of "child needs port at spawn time
/// but bind happens later"); the token must be live in the
/// `ChannelTokenTable` before the first upload could fire. `None`
/// disables the streaming path — every screenshot inlines.
///
/// Keeping `command` + `bundle_path` as plain inputs (rather than
/// reaching into `aura-gateway::SidecarRuntime` from here) is what
/// lets this module live in `aura-tools` without a dependency cycle.
#[allow(clippy::too_many_arguments)]
pub fn browser_mcp_profile(
    enable: bool,
    sandbox: bool,
    chrome_path: Option<&Path>,
    profile_dir: Option<&Path>,
    allow_loopback: bool,
    blob_upload: Option<BlobUploadEnv<'_>>,
    command: String,
    bundle_path: &Path,
) -> Option<EmbeddedMcpProfile> {
    if !enable {
        return None;
    }
    let mut extra_env: HashMap<String, String> = HashMap::new();
    if let Some(p) = profile_dir {
        extra_env.insert("AURA_BROWSER_PROFILE_DIR".into(), p.display().to_string());
    }
    if let Some(p) = chrome_path {
        extra_env.insert("AURA_CHROMIUM_BIN".into(), p.display().to_string());
    }
    // `sandbox: false` is the default; the TS sidecar reads
    // `AURA_BROWSER_NO_SANDBOX=1` to launch Chromium with `--no-sandbox`.
    // Set the env iff the operator left sandbox off — when they
    // explicitly enabled it, the child default (sandbox on) takes effect.
    if !sandbox {
        extra_env.insert("AURA_BROWSER_NO_SANDBOX".into(), "1".into());
    }
    if allow_loopback {
        extra_env.insert("AURA_BROWSER_ALLOW_LOOPBACK".into(), "1".into());
    }
    if let Some(up) = blob_upload {
        extra_env.insert(
            "AURA_CHANNEL_PORT_FILE".into(),
            up.port_file.display().to_string(),
        );
        extra_env.insert("AURA_BLOB_UPLOAD_TOKEN".into(), up.token.into());
    }
    Some(EmbeddedMcpProfile {
        server_name: "browser".into(),
        command,
        args: vec![bundle_path.display().to_string()],
        // Browser navigation can hit the network (Http) and JS eval
        // counts as ExecCommand. Per-tool `_meta.aura.access_rule`
        // refines what each call actually prompts for.
        capabilities: vec![ToolCapability::Http, ToolCapability::ExecCommand],
        extra_env,
    })
}

/// Where the browser MCP child should `POST /v1/blobs` for large
/// screenshots, and the token to authenticate with. The port itself
/// isn't passed — the child reads `port_file` lazily on first
/// upload, sidestepping the boot-order race.
#[derive(Debug, Clone, Copy)]
pub struct BlobUploadEnv<'a> {
    /// Path the channel TCP listener writes its bound port to (e.g.
    /// `<workspace>/state/channel.port`).
    pub port_file: &'a Path,
    /// Channel-token registered against a `tool/<sidecar>` label —
    /// `AuthedClient::Tool` bypasses pairing on `/v1/blobs`.
    pub token: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn disabled_yields_no_profile() {
        assert!(
            browser_mcp_profile(
                false,
                false,
                None,
                None,
                false,
                None,
                "node".into(),
                Path::new("/x.mjs"),
            )
            .is_none()
        );
    }

    #[test]
    fn enabled_synthesises_a_profile_with_no_sandbox_env() {
        let p = browser_mcp_profile(
            true,
            false,
            None,
            None,
            false,
            None,
            "node".into(),
            Path::new("/x.mjs"),
        )
        .expect("profile when enabled");
        assert_eq!(p.server_name, "browser");
        assert_eq!(p.command, "node");
        assert_eq!(p.args, vec!["/x.mjs".to_string()]);
        // sandbox=false default → AURA_BROWSER_NO_SANDBOX=1 in extra_env
        assert_eq!(p.extra_env.get("AURA_BROWSER_NO_SANDBOX").unwrap(), "1");
    }

    #[test]
    fn sandbox_on_clears_no_sandbox_env() {
        let p = browser_mcp_profile(
            true,
            true,
            None,
            None,
            false,
            None,
            "node".into(),
            Path::new("/x.mjs"),
        )
        .expect("profile when enabled");
        assert!(!p.extra_env.contains_key("AURA_BROWSER_NO_SANDBOX"));
    }

    #[test]
    fn chrome_path_lands_in_env() {
        let p = browser_mcp_profile(
            true,
            false,
            Some(&PathBuf::from("/opt/chrome")),
            None,
            false,
            None,
            "node".into(),
            Path::new("/x.mjs"),
        )
        .expect("profile when enabled");
        assert_eq!(p.extra_env.get("AURA_CHROMIUM_BIN").unwrap(), "/opt/chrome");
    }

    #[test]
    fn blob_upload_env_lands_in_env() {
        let port_file = PathBuf::from("/var/aura/state/channel.port");
        let p = browser_mcp_profile(
            true,
            false,
            None,
            None,
            false,
            Some(BlobUploadEnv {
                port_file: &port_file,
                token: "secret123",
            }),
            "node".into(),
            Path::new("/x.mjs"),
        )
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_CHANNEL_PORT_FILE").unwrap(),
            "/var/aura/state/channel.port"
        );
        assert_eq!(
            p.extra_env.get("AURA_BLOB_UPLOAD_TOKEN").unwrap(),
            "secret123"
        );
    }
}
