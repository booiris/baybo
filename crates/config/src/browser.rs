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
    /// Default: `<workspace_root>/work/.browser/profile` (computed by
    /// `aura_gateway::collect_profiles` from `workspace.path`). The
    /// profile is **persistent across Aura restarts** (cookies /
    /// localStorage retained) and lives under the workspace so it
    /// follows the operator's `workspace.path` and inherits the same
    /// gitignore + lifecycle as other `work/` state. In docker mode
    /// the same directory is bind-mounted at `/data/profile` inside
    /// the container, so the path round-trips across host-headless
    /// and docker modes (operator UID stays the owner).
    ///
    /// Note that chrome-devtools-mcp serialises browser access — only
    /// one Aura process can drive a given profile dir at a time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_dir: Option<PathBuf>,

    /// Run Chrome inside a Docker container with an Xvfb display so it
    /// presents as **non-headless** (better simulates a real user, dodges
    /// `HeadlessChrome` fingerprint checks). Opt-in. When Docker isn't
    /// available on the host (binary missing, daemon down, perms), the
    /// sidecar transparently falls back to the host-headless path so
    /// boot never fails on a missing daemon. See [`BrowserDockerConfig`].
    pub docker: BrowserDockerConfig,
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
            docker: BrowserDockerConfig::default(),
        }
    }
}

/// Docker-mode settings for the embedded browser sidecar.
///
/// When [`enable`](Self::enable) is `true` and Docker is available, the
/// sidecar spawns a container running Chrome behind Xvfb and connects
/// via CDP. When Docker is unavailable, the sidecar logs the reason
/// and falls back to the host-headless path — the gateway boot never
/// fails on a missing daemon.
///
/// In Docker mode [`BrowserConfig::sandbox`] is **ignored**: the
/// container itself is the isolation boundary, and Chrome's setuid
/// sandbox needs setup the slim image deliberately omits. The wrapper
/// logs an info line at boot if `sandbox=true` while docker mode is
/// active.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct BrowserDockerConfig {
    /// Master switch for Docker mode. **Default: `false`** (off — preserves
    /// today's host-headless behaviour exactly). Flipping to `true` makes
    /// the sidecar try to spawn a container; if Docker isn't available on
    /// the host, it logs a clear warning and falls back to host-headless
    /// rather than failing the gateway.
    ///
    /// **macOS exception**: even when set to `true`, the sidecar will
    /// fall through to host-headless on darwin. Docker Desktop on macOS
    /// runs Linux containers in a hidden VM, so the in-container Chrome
    /// is Linux Chrome behind that VM — not native macOS Chrome, which
    /// defeats the "real-user-simulation" point of the switch. macOS
    /// operators always get the host's native Chrome.
    pub enable: bool,

    /// Connect to a pre-existing Chrome's CDP endpoint instead of
    /// spawning a container. When set, the sidecar skips every Docker
    /// interaction (image build, container spawn, port mapping) and
    /// connects directly via [chrome-devtools-mcp]'s `browserUrl`. Use
    /// when running your own browser container under k8s / docker-
    /// compose / a remote host. Example: `http://127.0.0.1:9222`.
    ///
    /// Takes precedence over [`enable`](Self::enable) — `cdp_url` set
    /// means "I'm managing the browser; don't touch Docker."
    ///
    /// [chrome-devtools-mcp]: https://github.com/GoogleChrome/chrome-devtools-mcp
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cdp_url: Option<String>,

    /// When set, the spawned container runs `x11vnc` + `websockify` +
    /// the bundled noVNC HTML client on this port. Open
    /// `http://127.0.0.1:<port>/vnc.html` in any browser to watch the
    /// agent — no native VNC client needed. **Default: unset** (no VNC
    /// stack started, no port published).
    ///
    /// **No password by design.** The websockify HTTP/WS server binds
    /// to `127.0.0.1` inside the container and is only published on
    /// host-loopback — remote access requires an SSH tunnel (e.g.
    /// `ssh -L 6080:127.0.0.1:6080 host`, then open
    /// `http://127.0.0.1:6080/vnc.html`). It's a debugging primitive,
    /// not an exposed service. Don't publish to a public interface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_vnc_port: Option<u16>,

    /// Override the Docker image tag the sidecar runs. **Default: unset**,
    /// in which case the sidecar computes a deterministic tag of the form
    /// `aura-browser:<sha256(Dockerfile + entrypoint.sh + chrome_version)[..12]>`
    /// and builds the image on first boot if it isn't already present
    /// locally. Subsequent boots find the cached image (no rebuild) and
    /// new Aura versions land on a new tag (auto-rebuilds).
    ///
    /// Set this only to point at a hand-rolled image (e.g. an air-gapped
    /// registry mirror, a custom Chrome build). When set, the sidecar
    /// trusts the tag exists and skips the build entirely — `docker run`
    /// alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,
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
        assert!(
            !c.docker.enable,
            "docker mode is opt-in; default preserves host-headless behaviour"
        );
        assert!(c.docker.cdp_url.is_none());
        assert!(c.docker.web_vnc_port.is_none());
        assert!(c.docker.image_tag.is_none());
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
            docker: BrowserDockerConfig {
                enable: true,
                cdp_url: Some("http://127.0.0.1:9222".into()),
                web_vnc_port: Some(6080),
                image_tag: Some("custom/chrome:latest".into()),
            },
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
        assert!(
            !json.contains("cdp_url"),
            "None docker.cdp_url elided so the default block stays minimal"
        );
        assert!(
            !json.contains("web_vnc_port"),
            "None docker.web_vnc_port elided so the default block stays minimal"
        );
        assert!(
            !json.contains("image_tag"),
            "None docker.image_tag elided so the default block stays minimal"
        );
    }

    #[test]
    fn width_height_partial_override() {
        let c: BrowserConfig = serde_json::from_str(r#"{"width": 1280}"#).unwrap();
        assert_eq!(c.width, 1280, "width override applies");
        assert_eq!(c.height, 1080, "height keeps default 1080");
    }

    #[test]
    fn docker_disabled_by_default() {
        let c = BrowserConfig::default();
        assert!(
            !c.docker.enable,
            "docker.enable=false default — flipping to true must be an explicit operator opt-in",
        );
    }

    #[test]
    fn docker_enable_partial_override() {
        let c: BrowserConfig = serde_json::from_str(r#"{"docker": {"enable": true}}"#).unwrap();
        assert!(c.docker.enable);
        assert!(c.docker.cdp_url.is_none());
        assert!(c.docker.web_vnc_port.is_none());
        assert!(c.docker.image_tag.is_none());
    }

    #[test]
    fn docker_cdp_url_round_trips() {
        let c: BrowserConfig =
            serde_json::from_str(r#"{"docker": {"cdp_url": "http://10.0.0.5:9222"}}"#).unwrap();
        assert_eq!(c.docker.cdp_url.as_deref(), Some("http://10.0.0.5:9222"));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"cdp_url\":\"http://10.0.0.5:9222\""));
    }

    #[test]
    fn docker_web_vnc_port_round_trips() {
        let c: BrowserConfig =
            serde_json::from_str(r#"{"docker": {"web_vnc_port": 6080}}"#).unwrap();
        assert_eq!(c.docker.web_vnc_port, Some(6080));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"web_vnc_port\":6080"));
    }
}
