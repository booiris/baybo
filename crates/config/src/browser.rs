//! Configuration for the embedded browser MCP server (`sidecars/tool/browser`).
//!
//! The data lives here because `baybo-config` is the canonical
//! aggregator for `baybo.json` shape; the *builder* that turns this
//! into an [`baybo_tools::mcp::EmbeddedMcpProfile`] lives in
//! `baybo_tools::mcp::profile::browser` (alongside the MCP machinery
//! that consumes it). The gateway boot path unpacks the config fields
//! and hands them to `browser_mcp_profile` — keeping `baybo-config`
//! free of an `baybo-tools` runtime dep.

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
    /// `$XDG_CACHE_HOME/baybo/browser/chrome/` on first boot if it
    /// can't find a Chrome there.
    pub enable: bool,

    /// Path to the Chrome binary the sidecar drives.
    ///
    /// Default: unset, in which case the sidecar auto-downloads Google
    /// Chrome for Testing 'stable' into
    /// `$XDG_CACHE_HOME/baybo/browser/chrome/<platform>/chrome-<buildId>/`
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
    /// Default: `<workspace_root>/state/browser/profile` (computed by
    /// `baybo_gateway::collect_profiles` from `workspace.path`). The
    /// profile is **persistent across Baybo restarts** (cookies /
    /// localStorage retained) and lives under the workspace so it
    /// follows the operator's `workspace.path`. In docker mode the
    /// same directory is bind-mounted at `/data/profile` inside the
    /// container, so the path round-trips across host-headless and
    /// docker modes (operator UID stays the owner).
    ///
    /// Note that chrome-devtools-mcp serialises browser access — only
    /// one Baybo process can drive a given profile dir at a time.
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

/// See [`BrowserDockerConfig::memory_limit_mb`].
const DEFAULT_DOCKER_MEMORY_LIMIT_MB: u32 = 4096;

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// `baybo-browser:<sha256(Dockerfile + entrypoint.sh + chrome_version)[..12]>`
    /// and builds the image on first boot if it isn't already present
    /// locally. Subsequent boots find the cached image (no rebuild) and
    /// new Baybo versions land on a new tag (auto-rebuilds).
    ///
    /// Set this only to point at a hand-rolled image (e.g. an air-gapped
    /// registry mirror, a custom Chrome build). When set, the sidecar
    /// trusts the tag exists and skips the build entirely — `docker run`
    /// alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,

    /// Bind-mount `<workspace>/work` read-only inside the container so
    /// the agent can open artefacts it just wrote via `file://`.
    /// **Default: `true`.**
    ///
    /// Without it the container's filesystem carries only the Chrome
    /// profile and the font dir, and a `file://` URL naming any host path
    /// fails with `ERR_FILE_NOT_FOUND` — indistinguishable, from the
    /// model's side, from the file genuinely not existing.
    ///
    /// The mount is read-only: the agent has filesystem tools for
    /// writing, and read-only also keeps page JS from reaching back into
    /// the workspace through a `file://` origin. Set `false` to keep the
    /// container blind to the workspace entirely.
    pub mount_work_dir: bool,

    /// Hard memory ceiling for the container, in MiB, applied as
    /// `docker run --memory`/`--memory-swap`. **Default: `4096`**; set to
    /// `null` for no limit (the pre-cap behaviour).
    ///
    /// Chrome never returns a tab's memory on its own, so an uncapped
    /// container's ceiling is whatever the host has — and the host OOM
    /// killer picks the victim, possibly the gateway itself.
    ///
    /// A cap buys **containment, not recovery**. The cgroup OOM killer
    /// picks within the container, and Chrome renderers carry
    /// `oom_score_adj=300`, so a tab dies while the browser process and
    /// its CDP endpoint survive — the sidecar's watchdog probes liveness,
    /// sees a healthy browser, and correctly does not replace anything.
    /// The agent observes one unresponsive page; the host observes
    /// nothing.
    ///
    /// Swap is disabled for the container (`--memory-swap` is set equal
    /// to `--memory`): swapping a browser trades an honest OOM for
    /// minutes of thrash indistinguishable from a hang.
    ///
    /// Deliberately **not** `skip_serializing_if` — unlike every other
    /// `Option` here, `None` is not this field's default. `baybo config
    /// set` rewrites the whole file, so eliding the key would turn an
    /// operator's explicit "uncapped" back into `4096` the next time the
    /// config was read.
    pub memory_limit_mb: Option<u32>,
}

impl Default for BrowserDockerConfig {
    fn default() -> Self {
        Self {
            enable: false,
            cdp_url: None,
            web_vnc_port: None,
            image_tag: None,
            mount_work_dir: true,
            memory_limit_mb: Some(DEFAULT_DOCKER_MEMORY_LIMIT_MB),
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
        assert!(
            !c.docker.enable,
            "docker mode is opt-in; default preserves host-headless behaviour"
        );
        assert!(c.docker.cdp_url.is_none());
        assert!(c.docker.web_vnc_port.is_none());
        assert!(c.docker.image_tag.is_none());
        assert!(
            c.docker.mount_work_dir,
            "work dir is mounted by default — without it every file:// URL naming a host path \
             fails with ERR_FILE_NOT_FOUND and the model cannot tell that from a missing file",
        );
        assert_eq!(
            c.docker.memory_limit_mb,
            Some(DEFAULT_DOCKER_MEMORY_LIMIT_MB),
            "container is capped by default; Chrome never gives a tab's memory back",
        );
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
            profile_dir: Some(PathBuf::from("/tmp/baybo-profile")),
            docker: BrowserDockerConfig {
                enable: true,
                cdp_url: Some("http://127.0.0.1:9222".into()),
                web_vnc_port: Some(6080),
                image_tag: Some("custom/chrome:latest".into()),
                mount_work_dir: false,
                memory_limit_mb: Some(2048),
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
        assert!(
            c.docker.mount_work_dir,
            "a partial docker block must keep the other docker defaults",
        );
        assert_eq!(
            c.docker.memory_limit_mb,
            Some(DEFAULT_DOCKER_MEMORY_LIMIT_MB),
        );
    }

    /// `null` is the documented way to opt out of the memory cap, and it
    /// has to survive `#[serde(default)]` — which only fires for *absent*
    /// fields, not explicit nulls.
    #[test]
    fn explicit_null_memory_limit_means_uncapped() {
        let c: BrowserConfig =
            serde_json::from_str(r#"{"docker": {"memory_limit_mb": null}}"#).unwrap();
        assert_eq!(c.docker.memory_limit_mb, None);
    }

    /// `baybo config set` rewrites the whole file, so an explicit
    /// "uncapped" has to survive a serialise/deserialise cycle. It only
    /// does because `memory_limit_mb` is exempt from the
    /// `skip_serializing_if` the other `Option`s carry — for them `None`
    /// *is* the default, so eliding is lossless; here it would silently
    /// re-impose the 4096 cap the operator turned off.
    #[test]
    fn uncapped_memory_survives_a_config_rewrite() {
        let original: BrowserConfig =
            serde_json::from_str(r#"{"docker": {"memory_limit_mb": null}}"#).unwrap();
        let rewritten: BrowserConfig =
            serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
        assert_eq!(
            rewritten.docker.memory_limit_mb, None,
            "round-tripping the config must not turn an uncapped container back into a capped one",
        );
    }

    #[test]
    fn mount_work_dir_can_be_disabled() {
        let c: BrowserConfig =
            serde_json::from_str(r#"{"docker": {"mount_work_dir": false}}"#).unwrap();
        assert!(!c.docker.mount_work_dir);
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
