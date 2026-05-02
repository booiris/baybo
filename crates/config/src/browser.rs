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
    /// Master switch for the embedded browser MCP server (chrome-devtools-mcp).
    ///
    /// **Default: `false`** (off). When false, the sidecar is never
    /// spawned, the `browser/*` tools are not registered with the
    /// agent loop, and the bundle stays inert in the binary. Flip to
    /// `true` to opt the agent into web browsing — the sidecar will
    /// auto-download Google Chrome for Testing into
    /// `$XDG_CACHE_HOME/aura/browser/chrome/` on first boot if it
    /// can't find a Chrome there.
    pub enable: bool,

    /// Path to the Chrome binary the sidecar drives.
    ///
    /// Default: unset, in which case the sidecar auto-downloads Google
    /// Chrome for Testing 'stable' into
    /// `$XDG_CACHE_HOME/aura/browser/chrome/<platform>/chrome-<buildId>/`
    /// on first boot and uses that path. Set this only to pin to a
    /// specific Chrome (e.g. a system binary, a custom build, an
    /// air-gapped vendor distribution). Chrome for Testing is the
    /// same Blink/V8/codecs/Widevine stack as consumer Chrome, just
    /// repackaged for automation; not Chromium (the open-source
    /// upstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chrome_path: Option<PathBuf>,

    /// Run Chrome with its renderer sandbox.
    ///
    /// **Default: `false`** (sandbox off, `--no-sandbox` passed). Many
    /// container/CI environments can't satisfy Chrome's user-namespace
    /// prerequisites and the browser otherwise refuses to start with
    /// "Failed to move to new namespace" or similar errors. Flip to
    /// `true` once you've verified the host supports the sandbox
    /// (typical Linux desktop, rootful docker with the `chrome-sandbox`
    /// SUID binary in place, …) — the renderer sandbox is the floor
    /// between attacker-controlled page code and the gateway user, so
    /// turn it on whenever the host allows. Has no effect when
    /// `enable=false`.
    pub sandbox: bool,

    /// Initial viewport width in CSS pixels.
    ///
    /// **Default: `1920`**. Together with [`height`](#structfield.height)
    /// pins every new tab to the configured viewport. Override for
    /// mobile emulation (e.g. `390` × `844`) or denser layouts. In
    /// headless mode the practical max is 3840 × 2160.
    pub width: u32,

    /// Initial viewport height in CSS pixels. **Default: `1080`**. See
    /// [`width`](#structfield.width).
    pub height: u32,

    /// Override the per-sidecar Chrome profile directory.
    ///
    /// Default: `$XDG_CACHE_HOME/aura/browser/profile`. The profile is
    /// **persistent across Aura restarts** (cookies / localStorage
    /// retained) but kept separate from the user's normal Chrome
    /// profile by virtue of being under Aura's cache root. Note that
    /// chrome-devtools-mcp serialises browser access — only one Aura
    /// process can drive this profile dir at a time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_dir: Option<PathBuf>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enable: false,
            chrome_path: None,
            sandbox: false,
            width: 1920,
            height: 1080,
            profile_dir: None,
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
        assert!(
            !c.sandbox,
            "sandbox defaults off (matches container/CI hosts)"
        );
        assert!(c.chrome_path.is_none());
        assert!(c.profile_dir.is_none());
        assert_eq!(c.width, 1920);
        assert_eq!(c.height, 1080);
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
            chrome_path: Some(PathBuf::from("/opt/google/chrome/chrome")),
            sandbox: true,
            width: 1280,
            height: 720,
            profile_dir: Some(PathBuf::from("/tmp/aura-profile")),
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
    }

    #[test]
    fn width_height_partial_override() {
        let c: BrowserConfig = serde_json::from_str(r#"{"width": 1280}"#).unwrap();
        assert_eq!(c.width, 1280, "width override applies");
        assert_eq!(c.height, 1080, "height keeps default 1080");
    }
}
