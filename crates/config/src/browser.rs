//! Configuration for the embedded browser MCP server (`tool-src/browser`).
//!
//! The data lives here because `aura-config` is the canonical
//! aggregator for `aura.json` shape; the *builder* that turns this
//! into an [`aura_tools::mcp::EmbeddedMcpProfile`] lives in
//! `aura_tools::mcp::profile::browser` (alongside the MCP machinery
//! that consumes it). The gateway boot path unpacks the config fields
//! and hands them to `browser_mcp_profile` — keeping `aura-config`
//! free of an `aura-tools` runtime dep.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BrowserConfig {
    /// Master switch for the embedded browser MCP server.
    ///
    /// **Default: `false`** (off). When false, the sidecar is never
    /// spawned, the `browser/*` tools are not registered with the
    /// agent loop, and the bundle stays inert in the binary. Flip to
    /// `true` to opt the agent into web browsing — at which point you
    /// also need to confirm `sandbox` (below) is appropriate for the
    /// host and run `pnpm --filter @aura/tool-browser exec playwright
    /// install chromium` once.
    pub enable: bool,

    /// Run Chromium with its renderer sandbox.
    ///
    /// **Default: `false`** (sandbox off). Many container/CI environments
    /// can't satisfy Chromium's user-namespace prerequisites and the
    /// browser otherwise refuses to start. Enable this when you can
    /// verify the sandbox actually works (typical Linux desktop, rootful
    /// docker with the proper SUID `chrome-sandbox` binary, …) — the
    /// renderer sandbox is the floor between attacker-controlled page
    /// code and the gateway user. Has no effect when `enable=false`.
    pub sandbox: bool,

    /// Override the Chromium binary the sidecar drives.
    ///
    /// Default: Playwright's bundled Chromium under
    /// `tool-src/browser/node_modules/...`. Set this to point at a
    /// system Chrome / Chromium when you need a specific build (host
    /// fonts, custom Widevine, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chrome_path: Option<PathBuf>,

    /// Override the per-sidecar Chromium profile directory.
    ///
    /// Default: `$XDG_CACHE_HOME/aura/browser/profile`. The sidecar
    /// refuses to start if the path resolves under any platform default
    /// browser profile root (Chrome, Firefox, Brave, …) — the agent's
    /// browsing data must never bleed into the user's normal browser.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_dir: Option<PathBuf>,

    /// Extra Chromium command-line arguments, appended verbatim to
    /// the launch flags. **Each entry must include the leading `-` /
    /// `--`** — Aura passes them through without mangling, so write
    /// `"--disable-features=BlockInsecurePrivateNetworkRequests"`,
    /// not `"disable-features=..."`.
    ///
    /// Default: empty. The sidecar always passes `--disable-gpu` (and,
    /// when `sandbox = false`, `--no-sandbox`); these don't need to
    /// be repeated here. Use this for site-specific workarounds
    /// (e.g. `"--lang=en-US"`, `"--ignore-certificate-errors"` —
    /// the latter at your own risk).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    /// Test-only escape hatch: admit navigations to 127.0.0.0/8 and
    /// `::1`. Mirrors `aura_security::is_blocked_ip(allow_loopback)`
    /// so smoke tests can bind a local HTTP server and drive a real
    /// Chromium against it. **Production must leave this off** — the
    /// SSRF floor depends on it.
    pub allow_loopback: bool,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enable: false,
            sandbox: false,
            chrome_path: None,
            profile_dir: None,
            args: Vec::new(),
            allow_loopback: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let c = BrowserConfig::default();
        assert!(
            !c.enable,
            "enable defaults off (opt-in like gateway.enabled)"
        );
        assert!(!c.sandbox, "sandbox defaults off");
        assert!(c.chrome_path.is_none());
        assert!(c.profile_dir.is_none());
        assert!(c.args.is_empty(), "no extra Chromium args by default");
        assert!(!c.allow_loopback, "allow_loopback defaults off");
    }

    #[test]
    fn empty_object_yields_defaults() {
        let c: BrowserConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c, BrowserConfig::default());
    }

    #[test]
    fn round_trip() {
        let c = BrowserConfig {
            enable: true,
            sandbox: true,
            chrome_path: Some(PathBuf::from("/usr/bin/chromium")),
            profile_dir: Some(PathBuf::from("/tmp/aura-profile")),
            args: vec!["--lang=en-US".into(), "--ignore-certificate-errors".into()],
            allow_loopback: true,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: BrowserConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn omits_none_paths() {
        let c = BrowserConfig::default();
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("chrome_path"), "None chrome_path elided");
        assert!(!json.contains("profile_dir"), "None profile_dir elided");
        assert!(!json.contains("args"), "empty args elided");
    }
}
