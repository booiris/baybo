//! Browser embedded MCP server: profile builder.
//!
//! The user-facing config struct lives in `baybo_config::BrowserConfig`
//! (so `baybo.json` deserialisation has no `baybo-tools` runtime dep);
//! this module owns the *policy translation* — turning the operator's
//! settings into the concrete env vars + capability list the embedded
//! MCP child consumes.
//!
//! [`browser_mcp_profile`] takes primitive args rather than a typed
//! config struct so this crate stays independent of `baybo-config`. The
//! gateway boot path does the trivial unpacking (`config.browser.enable`,
//! `config.browser.chrome_path`, `config.browser.sandbox`, …).
//!
//! The TS sidecar at `sidecars/tool/browser/src/server.ts` is a thin wrapper
//! around `chrome-devtools-mcp` (pinned to a fixed version under
//! `sidecars/tool/browser/package.json`). It reads internal IPC env vars
//! plumbed by this builder and forces telemetry off, headless on, and
//! `isolated=false` so the userDataDir persists across restarts.

use std::collections::HashMap;
use std::path::Path;

use super::EmbeddedMcpProfile;
use crate::ToolTriggerScope;

/// Policy inputs for [`browser_mcp_profile`].
///
/// Deliberately primitives + `Path`s rather than `baybo_config` types, so
/// this crate stays free of a config dep; the gateway does the unpacking.
/// A struct rather than a positional arg list because the call site is
/// otherwise fifteen bare values in a row, where a transposed pair of
/// `bool`s type-checks and silently inverts a policy.
pub struct BrowserProfileParams<'a> {
    /// Master switch — `false` makes the builder return `None`.
    pub enable: bool,
    pub chrome_path: Option<&'a Path>,
    pub profile_dir: Option<&'a Path>,
    pub sandbox: bool,
    pub width: u32,
    pub height: u32,
    /// Typically the resolved host `node` binary.
    pub command: String,
    /// The materialised `dist/bundle.mjs` path.
    pub bundle_path: &'a Path,
    pub extra_font_dirs: &'a [&'a Path],
    pub docker_enable: bool,
    pub docker_cdp_url: Option<&'a str>,
    pub docker_web_vnc_port: Option<u16>,
    pub docker_image_tag: Option<&'a str>,
    /// Host directory bind-mounted read-only in the container so `file://`
    /// URLs for agent-written artefacts resolve. `None` leaves the
    /// container with no view of the workspace.
    pub docker_work_dir: Option<&'a Path>,
    /// Container memory ceiling in MiB. `None` leaves it uncapped.
    pub docker_memory_limit_mb: Option<u32>,
}

/// Build the `browser` MCP-server profile from the operator's policy
/// settings. Returns `None` when `enable=false` so the boot path drops
/// it uniformly with the rest of the disabled profiles.
///
/// Fields of [`BrowserProfileParams`]:
/// - `enable`: master switch — `false` returns `None`.
/// - `chrome_path`: optional override for the Chrome binary, plumbed
///   via the internal `BAYBO_BROWSER_CHROME_PATH` env var. When unset,
///   the sidecar auto-downloads Google Chrome for Testing 'stable'
///   into `$XDG_CACHE_HOME/baybo/browser/chrome/` on first boot and
///   uses that path; subsequent boots find the cached binary
///   synchronously.
/// - `profile_dir`: optional override for the Chrome user-data dir,
///   plumbed via `BAYBO_BROWSER_PROFILE_DIR`.
/// - `sandbox`: when `false`, sets `BAYBO_BROWSER_NO_SANDBOX=1` so the
///   TS sidecar appends `--no-sandbox` to Chrome's launch args. Default
///   off because most container/CI hosts can't satisfy Chrome's
///   user-namespace prerequisites.
/// - `width`, `height`: initial viewport size, plumbed as
///   `BAYBO_BROWSER_VIEWPORT=<W>x<H>`. The TS wrapper passes this to
///   CDDM as `viewport: { width, height }`.
/// - `command`: typically the resolved host `node` binary
///   (`baybo_gateway::node_binary().display().to_string()`).
/// - `bundle_path`: the materialised `dist/bundle.mjs` path
///   (`runtime.bundle_for("browser")`).
/// - `extra_font_dirs`: directories to add to the Chrome's fontconfig
///   search path. Joined with `:` and plumbed via
///   `BAYBO_BROWSER_EXTRA_FONT_DIRS`. The TS wrapper writes a fontconfig
///   include file and sets `FONTCONFIG_FILE` before loading CDDM, so
///   Chrome (which is spawned in-process by CDDM/puppeteer) inherits
///   the augmented font search path. In docker mode the wrapper
///   bind-mounts the *first existing* dir at `/data/fonts` instead.
///   Empty slice = no override.
/// - `docker_enable`: master switch for docker mode. When true and
///   docker is available on the host, the TS wrapper spawns a Chrome-
///   in-Xvfb container and connects via CDP (`browserUrl`); when
///   docker is *not* available, the wrapper falls back to the host-
///   headless path so boot still succeeds. Plumbed as
///   `BAYBO_BROWSER_DOCKER_ENABLE=1`.
/// - `docker_cdp_url`: when set, take precedence over `docker_enable`
///   and connect to a pre-existing Chrome (operator-managed
///   container, k8s pod, …) — sidecar performs zero docker
///   interaction. Plumbed as `BAYBO_BROWSER_DOCKER_CDP_URL`.
/// - `docker_web_vnc_port`: when set, the spawned container runs
///   `x11vnc` + `websockify` + the bundled noVNC HTML client on this
///   port for browser-based observability. Plumbed as
///   `BAYBO_BROWSER_DOCKER_WEB_VNC_PORT`.
/// - `docker_image_tag`: when set, skip the deterministic-tag computation
///   + image build and trust this tag exists. Plumbed as
///     `BAYBO_BROWSER_DOCKER_IMAGE_TAG`.
/// - `docker_work_dir`: host directory bind-mounted read-only inside the
///   container, plumbed as `BAYBO_BROWSER_DOCKER_WORK_DIR`. Without it a
///   `file://` URL naming a host path fails with `ERR_FILE_NOT_FOUND`,
///   which the model reads as "the file isn't there".
/// - `docker_memory_limit_mb`: container memory ceiling, plumbed as
///   `BAYBO_BROWSER_DOCKER_MEMORY_LIMIT` in docker's own `<N>m` syntax.
///   Ignored outside docker mode.
///
/// `capabilities` is intentionally empty: dropping the
/// `[Http, ExecCommand]` ceiling means `accessed_resources()` returns
/// `[]` for every browser tool call, which short-circuits the agent
/// loop's pre-execute approval gate. Baybo *trusts* the embedded
/// browser sidecar to make navigation/JS-eval decisions on the agent's
/// behalf — the gate is off by design here.
///
/// Keeping `command` + `bundle_path` as plain inputs (rather than
/// reaching into `baybo-gateway::SidecarRuntime` from here) is what
/// lets this module live in `baybo-tools` without a dependency cycle.
pub fn browser_mcp_profile(params: BrowserProfileParams<'_>) -> Option<EmbeddedMcpProfile> {
    let BrowserProfileParams {
        enable,
        chrome_path,
        profile_dir,
        sandbox,
        width,
        height,
        command,
        bundle_path,
        extra_font_dirs,
        docker_enable,
        docker_cdp_url,
        docker_web_vnc_port,
        docker_image_tag,
        docker_work_dir,
        docker_memory_limit_mb,
    } = params;
    if !enable {
        return None;
    }
    let mut extra_env: HashMap<String, String> = HashMap::new();
    if let Some(p) = profile_dir {
        extra_env.insert("BAYBO_BROWSER_PROFILE_DIR".into(), p.display().to_string());
    }
    if let Some(p) = chrome_path {
        extra_env.insert("BAYBO_BROWSER_CHROME_PATH".into(), p.display().to_string());
    }
    // sandbox=false (the default) → tell the TS sidecar to launch
    // Chrome with `--no-sandbox`. We use a presence-only env: set when
    // disabled, absent when enabled. Mirrors the previous sandbox
    // plumbing under `BAYBO_BROWSER_NO_SANDBOX`.
    if !sandbox {
        extra_env.insert("BAYBO_BROWSER_NO_SANDBOX".into(), "1".into());
    }
    // Viewport: the TS wrapper's `parseViewport` accepts `<W>x<H>`.
    // Always set; the BrowserConfig defaults guarantee non-zero
    // values (1920×1080) and the operator can override either
    // dimension independently in `baybo.json`.
    extra_env.insert("BAYBO_BROWSER_VIEWPORT".into(), format!("{width}x{height}"));
    // Belt-and-braces telemetry suppression. The TS wrapper already
    // passes `usageStatistics: false` programmatically, but CDDM's CLI
    // also reads `CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS` and `CI`.
    // Setting both keeps telemetry off even if a future CDDM upgrade
    // changes the default flag plumbing.
    extra_env.insert("CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS".into(), "1".into());
    extra_env.insert("CI".into(), "1".into());
    // Silence Node startup warnings. Lighthouse (bundled inside CDDM
    // for `lighthouse_audit`) transitively imports `debug`'s browser
    // variant, which touches `globalThis.localStorage` at module-init
    // and triggers Node 22+'s "`--localstorage-file` was provided
    // without a valid path" warning. The warning is cosmetic
    // (`debug`'s try/catch reads the failure as "no debug logging"
    // and proceeds) but it pollutes the gateway's captured stderr
    // tracing buffer. A `process.on('warning', ...)` filter on the JS
    // side does NOT suppress the default printer in Node 22+ (the
    // listener fires *in addition to* the built-in printer); using
    // `--no-warnings` via NODE_OPTIONS is the only knob that actually
    // silences it. We accept losing other Node warnings: this is a
    // wrapper around opaque vendor code (CDDM + Lighthouse +
    // Puppeteer); future Node deprecations on those transitive deps
    // are not actionable for Baybo operators anyway.
    extra_env.insert("NODE_OPTIONS".into(), "--no-warnings".into());
    if !extra_font_dirs.is_empty() {
        let joined = extra_font_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        extra_env.insert("BAYBO_BROWSER_EXTRA_FONT_DIRS".into(), joined);
    }
    // Docker mode plumbing. All four are presence-only / value-bearing
    // env vars the TS wrapper reads at startup. `cdp_url` takes
    // precedence: when set, the wrapper skips docker entirely and
    // connects to the operator's Chrome.
    if docker_enable {
        extra_env.insert("BAYBO_BROWSER_DOCKER_ENABLE".into(), "1".into());
    }
    if let Some(url) = docker_cdp_url {
        extra_env.insert("BAYBO_BROWSER_DOCKER_CDP_URL".into(), url.into());
    }
    if let Some(port) = docker_web_vnc_port {
        extra_env.insert("BAYBO_BROWSER_DOCKER_WEB_VNC_PORT".into(), port.to_string());
    }
    if let Some(tag) = docker_image_tag {
        extra_env.insert("BAYBO_BROWSER_DOCKER_IMAGE_TAG".into(), tag.into());
    }
    // Unlike `cdp_url` — which the wrapper honours whether or not docker
    // mode is on — these two describe a container that only exists when
    // `docker_enable` is set. Emitting them regardless would put a
    // container bind-mount path and a `--memory` value in the env of a
    // host-headless child, where the first thing to read them would take
    // a docker-only code path in a mode that has no container.
    if docker_enable {
        if let Some(dir) = docker_work_dir {
            extra_env.insert(
                "BAYBO_BROWSER_DOCKER_WORK_DIR".into(),
                dir.display().to_string(),
            );
        }
        // Docker's own size suffix — the wrapper forwards the string to
        // `--memory` verbatim rather than re-deriving a unit.
        if let Some(mb) = docker_memory_limit_mb {
            extra_env.insert("BAYBO_BROWSER_DOCKER_MEMORY_LIMIT".into(), format!("{mb}m"));
        }
    }
    Some(EmbeddedMcpProfile {
        server_name: "browser".into(),
        command,
        args: vec![bundle_path.display().to_string()],
        // One Chrome, one profile, one cookie jar for the whole process —
        // so the sessions that share the workspace can share it, and the
        // card runs that were cut their own checkout cannot. Keeping it off
        // them also takes 24 tools / ~19 KB off every request they make.
        trigger_scope: ToolTriggerScope::SharedWorkspace,
        // Empty by design — see the doc comment above. The McpTool
        // wrapper's `accessed_resources()` returns this list verbatim,
        // so an empty capability ceiling means the agent loop's
        // pre-execute approval gate never fires for browser tool calls.
        capabilities: Vec::new(),
        extra_env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `sandbox=false` matches the `BrowserConfig` default and exercises
    /// the BAYBO_BROWSER_NO_SANDBOX-on path; the override case is
    /// covered by `sandbox_on_clears_no_sandbox_env` below.
    fn params(enable: bool) -> BrowserProfileParams<'static> {
        BrowserProfileParams {
            enable,
            chrome_path: None,
            profile_dir: None,
            sandbox: false,
            width: 1920,
            height: 1080,
            command: "node".into(),
            bundle_path: Path::new("/x.mjs"),
            extra_font_dirs: &[],
            docker_enable: false,
            docker_cdp_url: None,
            docker_web_vnc_port: None,
            docker_image_tag: None,
            // Deliberately `Some` even though the fixture is docker-off:
            // these two default to a real value in `baybo.json`, so a
            // fixture that left them `None` would make
            // `docker_disabled_omits_all_docker_env` pass no matter what
            // the builder did.
            docker_work_dir: Some(Path::new("/ws/work")),
            docker_memory_limit_mb: Some(4096),
        }
    }

    fn defaults_call(enable: bool) -> Option<EmbeddedMcpProfile> {
        browser_mcp_profile(params(enable))
    }

    #[test]
    fn disabled_yields_no_profile() {
        assert!(defaults_call(false).is_none());
    }

    #[test]
    fn enabled_synthesises_a_profile_with_telemetry_off_and_no_sandbox() {
        let p = defaults_call(true).expect("profile when enabled");
        assert_eq!(p.server_name, "browser");
        assert_eq!(p.command, "node");
        assert_eq!(p.args, vec!["/x.mjs".to_string()]);
        assert!(
            p.capabilities.is_empty(),
            "capabilities=[] so the agent loop's approval gate is bypassed for browser/*",
        );
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_NO_SANDBOX"),
            Some(&"1".to_string()),
            "sandbox=false default → BAYBO_BROWSER_NO_SANDBOX=1 so the TS sidecar appends --no-sandbox",
        );
        assert_eq!(
            p.extra_env.get("CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS"),
            Some(&"1".to_string()),
        );
        assert_eq!(p.extra_env.get("CI"), Some(&"1".to_string()));
        assert_eq!(
            p.extra_env.get("NODE_OPTIONS"),
            Some(&"--no-warnings".to_string()),
            "Lighthouse transitively pokes globalThis.localStorage on module-init and \
             triggers Node 22+'s --localstorage-file warning; --no-warnings is the only \
             knob that actually silences it (process.on('warning') doesn't replace the \
             default printer in Node 22+).",
        );
    }

    /// Pinned guarantee: browser tools must never trigger the agent
    /// loop's pre-execute approval prompt. Enforced by
    /// `capabilities: vec![]` here — McpTool's `accessed_resources()`
    /// returns the per-server list verbatim, so empty capabilities =
    /// no prompt. If a future refactor sneaks Http or ExecCommand into
    /// the browser profile's capability list, every `browser/*` tool
    /// call would start prompting and silently degrade UX. This test
    /// fails loud if that happens.
    #[test]
    fn capabilities_stay_empty_to_skip_approval_gate() {
        let p = defaults_call(true).expect("profile when enabled");
        assert!(
            p.capabilities.is_empty(),
            "browser profile MUST keep capabilities=[] — adding any \
             ToolCapability would re-enable the agent loop's approval gate \
             for every browser/* tool call. See the doc comment on \
             browser_mcp_profile for the full reasoning.",
        );
    }

    #[test]
    fn sandbox_on_clears_no_sandbox_env() {
        let p = browser_mcp_profile(BrowserProfileParams {
            sandbox: true,
            ..params(true)
        })
        .expect("profile when enabled");
        assert!(
            !p.extra_env.contains_key("BAYBO_BROWSER_NO_SANDBOX"),
            "sandbox=true must NOT set BAYBO_BROWSER_NO_SANDBOX so Chrome launches with its sandbox on",
        );
    }

    #[test]
    fn chrome_path_lands_in_env() {
        let chrome = PathBuf::from("/opt/google/chrome/chrome");
        let p = browser_mcp_profile(BrowserProfileParams {
            chrome_path: Some(&chrome),
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_CHROME_PATH").unwrap(),
            "/opt/google/chrome/chrome"
        );
    }

    #[test]
    fn profile_dir_lands_in_env() {
        let dir = PathBuf::from("/var/baybo/browser-profile");
        let p = browser_mcp_profile(BrowserProfileParams {
            profile_dir: Some(&dir),
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_PROFILE_DIR").unwrap(),
            "/var/baybo/browser-profile"
        );
    }

    #[test]
    fn viewport_lands_in_env_as_wxh() {
        let p = browser_mcp_profile(BrowserProfileParams {
            width: 1280,
            height: 720,
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_VIEWPORT").unwrap(),
            "1280x720",
            "viewport encoded as <W>x<H> for the TS wrapper's parseViewport",
        );
    }

    #[test]
    fn viewport_default_lands_in_env() {
        let p = defaults_call(true).expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_VIEWPORT").unwrap(),
            "1920x1080",
        );
    }

    #[test]
    fn empty_font_dirs_omits_env() {
        let p = defaults_call(true).expect("profile when enabled");
        assert!(
            !p.extra_env.contains_key("BAYBO_BROWSER_EXTRA_FONT_DIRS"),
            "empty extra_font_dirs must NOT set the env var so the TS sidecar skips fontconfig override entirely",
        );
    }

    #[test]
    fn font_dirs_join_with_colon() {
        let a = PathBuf::from("/work/.fonts");
        let b = PathBuf::from("/usr/share/extra-fonts");
        let dirs = [a.as_path(), b.as_path()];
        let p = browser_mcp_profile(BrowserProfileParams {
            extra_font_dirs: &dirs,
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_EXTRA_FONT_DIRS").unwrap(),
            "/work/.fonts:/usr/share/extra-fonts",
            "multiple font dirs encoded as colon-joined path list (PATH-style) for the TS wrapper",
        );
    }

    #[test]
    fn docker_disabled_omits_all_docker_env() {
        let p = defaults_call(true).expect("profile when enabled");
        for key in [
            "BAYBO_BROWSER_DOCKER_ENABLE",
            "BAYBO_BROWSER_DOCKER_CDP_URL",
            "BAYBO_BROWSER_DOCKER_WEB_VNC_PORT",
            "BAYBO_BROWSER_DOCKER_IMAGE_TAG",
            "BAYBO_BROWSER_DOCKER_WORK_DIR",
            "BAYBO_BROWSER_DOCKER_MEMORY_LIMIT",
        ] {
            assert!(
                !p.extra_env.contains_key(key),
                "docker mode off must omit {key} so the TS wrapper takes the host-headless path",
            );
        }
    }

    #[test]
    fn docker_enable_lands_in_env() {
        let p = browser_mcp_profile(BrowserProfileParams {
            docker_enable: true,
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_DOCKER_ENABLE"),
            Some(&"1".to_string()),
            "docker_enable=true → BAYBO_BROWSER_DOCKER_ENABLE=1 (presence-only flag)",
        );
    }

    #[test]
    fn docker_cdp_url_lands_in_env() {
        let p = browser_mcp_profile(BrowserProfileParams {
            docker_cdp_url: Some("http://10.0.0.5:9222"),
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_DOCKER_CDP_URL").unwrap(),
            "http://10.0.0.5:9222",
        );
    }

    #[test]
    fn docker_image_tag_override_lands_in_env() {
        let p = browser_mcp_profile(BrowserProfileParams {
            docker_enable: true,
            docker_image_tag: Some("my-registry/baybo-browser:pinned"),
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_DOCKER_IMAGE_TAG").unwrap(),
            "my-registry/baybo-browser:pinned",
            "operator-supplied tag short-circuits the deterministic tag computation in the wrapper",
        );
    }

    #[test]
    fn docker_web_vnc_port_lands_in_env_as_string() {
        let p = browser_mcp_profile(BrowserProfileParams {
            docker_enable: true,
            docker_web_vnc_port: Some(6080),
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env
                .get("BAYBO_BROWSER_DOCKER_WEB_VNC_PORT")
                .unwrap(),
            "6080",
            "web_vnc_port serialised to a decimal string for the TS wrapper to parse",
        );
    }

    #[test]
    fn docker_work_dir_lands_in_env() {
        let work = PathBuf::from("/var/baybo/work");
        let p = browser_mcp_profile(BrowserProfileParams {
            docker_enable: true,
            docker_work_dir: Some(&work),
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("BAYBO_BROWSER_DOCKER_WORK_DIR").unwrap(),
            "/var/baybo/work",
        );
    }

    /// The wrapper forwards this string to `docker run --memory` verbatim,
    /// so the unit suffix has to be docker's, not a bare integer.
    #[test]
    fn docker_memory_limit_serialises_with_docker_suffix() {
        let p = browser_mcp_profile(BrowserProfileParams {
            docker_enable: true,
            docker_memory_limit_mb: Some(4096),
            ..params(true)
        })
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env
                .get("BAYBO_BROWSER_DOCKER_MEMORY_LIMIT")
                .unwrap(),
            "4096m",
        );
    }

    #[test]
    fn no_memory_limit_omits_env() {
        let p = browser_mcp_profile(BrowserProfileParams {
            docker_enable: true,
            docker_memory_limit_mb: None,
            ..params(true)
        })
        .expect("profile when enabled");
        assert!(
            !p.extra_env
                .contains_key("BAYBO_BROWSER_DOCKER_MEMORY_LIMIT"),
            "None means uncapped — the wrapper must not receive a --memory value at all",
        );
    }

    #[test]
    fn the_browser_is_offered_to_every_session_but_a_card_s_run() {
        let p = browser_mcp_profile(params(true)).expect("profile when enabled");
        assert_eq!(p.trigger_scope, ToolTriggerScope::SharedWorkspace);
    }
}
