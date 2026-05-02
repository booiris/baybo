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
//! `config.browser.chrome_path`, `config.browser.sandbox`, …).
//!
//! The TS sidecar at `tool-src/browser/src/server.ts` is a thin wrapper
//! around `chrome-devtools-mcp` (pinned to a fixed version under
//! `tool-src/browser/package.json`). It reads internal IPC env vars
//! plumbed by this builder and forces telemetry off, headless on, and
//! `isolated=false` so the userDataDir persists across restarts.

use std::collections::HashMap;
use std::path::Path;

use super::EmbeddedMcpProfile;

/// Build the `browser` MCP-server profile from the operator's policy
/// settings. Returns `None` when `enable=false` so the boot path drops
/// it uniformly with the rest of the disabled profiles.
///
/// Args:
/// - `enable`: master switch — `false` returns `None`.
/// - `chrome_path`: optional override for the Chrome binary, plumbed
///   via the internal `AURA_BROWSER_CHROME_PATH` env var. When unset,
///   the sidecar auto-downloads Google Chrome for Testing 'stable'
///   into `$XDG_CACHE_HOME/aura/browser/chrome/` on first boot and
///   uses that path; subsequent boots find the cached binary
///   synchronously.
/// - `profile_dir`: optional override for the Chrome user-data dir,
///   plumbed via `AURA_BROWSER_PROFILE_DIR`.
/// - `sandbox`: when `false`, sets `AURA_BROWSER_NO_SANDBOX=1` so the
///   TS sidecar appends `--no-sandbox` to Chrome's launch args. Default
///   off because most container/CI hosts can't satisfy Chrome's
///   user-namespace prerequisites.
/// - `width`, `height`: initial viewport size, plumbed as
///   `AURA_BROWSER_VIEWPORT=<W>x<H>`. The TS wrapper passes this to
///   CDDM as `viewport: { width, height }`.
/// - `command`: typically the resolved host `node` binary
///   (`aura_gateway::node_binary().display().to_string()`).
/// - `bundle_path`: the materialised `dist/bundle.mjs` path
///   (`runtime.bundle_for("browser")`).
/// - `extra_font_dirs`: directories to add to the Chrome's fontconfig
///   search path. Joined with `:` and plumbed via
///   `AURA_BROWSER_EXTRA_FONT_DIRS`. The TS wrapper writes a fontconfig
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
///   `AURA_BROWSER_DOCKER_ENABLE=1`.
/// - `docker_cdp_url`: when set, take precedence over `docker_enable`
///   and connect to a pre-existing Chrome (operator-managed
///   container, k8s pod, …) — sidecar performs zero docker
///   interaction. Plumbed as `AURA_BROWSER_DOCKER_CDP_URL`.
/// - `docker_web_vnc_port`: when set, the spawned container runs
///   `x11vnc` + `websockify` + the bundled noVNC HTML client on this
///   port for browser-based observability. Plumbed as
///   `AURA_BROWSER_DOCKER_WEB_VNC_PORT`.
/// - `docker_image_tag`: when set, skip the deterministic-tag computation
///   + image build and trust this tag exists. Plumbed as
///     `AURA_BROWSER_DOCKER_IMAGE_TAG`.
///
/// `capabilities` is intentionally empty: dropping the
/// `[Http, ExecCommand]` ceiling means `accessed_resources()` returns
/// `[]` for every browser tool call, which short-circuits the agent
/// loop's pre-execute approval gate. Aura *trusts* the embedded
/// browser sidecar to make navigation/JS-eval decisions on the agent's
/// behalf — the gate is off by design here.
///
/// Keeping `command` + `bundle_path` as plain inputs (rather than
/// reaching into `aura-gateway::SidecarRuntime` from here) is what
/// lets this module live in `aura-tools` without a dependency cycle.
#[allow(clippy::too_many_arguments)]
pub fn browser_mcp_profile(
    enable: bool,
    chrome_path: Option<&Path>,
    profile_dir: Option<&Path>,
    sandbox: bool,
    width: u32,
    height: u32,
    command: String,
    bundle_path: &Path,
    extra_font_dirs: &[&Path],
    docker_enable: bool,
    docker_cdp_url: Option<&str>,
    docker_web_vnc_port: Option<u16>,
    docker_image_tag: Option<&str>,
) -> Option<EmbeddedMcpProfile> {
    if !enable {
        return None;
    }
    let mut extra_env: HashMap<String, String> = HashMap::new();
    if let Some(p) = profile_dir {
        extra_env.insert("AURA_BROWSER_PROFILE_DIR".into(), p.display().to_string());
    }
    if let Some(p) = chrome_path {
        extra_env.insert("AURA_BROWSER_CHROME_PATH".into(), p.display().to_string());
    }
    // sandbox=false (the default) → tell the TS sidecar to launch
    // Chrome with `--no-sandbox`. We use a presence-only env: set when
    // disabled, absent when enabled. Mirrors the previous sandbox
    // plumbing under `AURA_BROWSER_NO_SANDBOX`.
    if !sandbox {
        extra_env.insert("AURA_BROWSER_NO_SANDBOX".into(), "1".into());
    }
    // Viewport: the TS wrapper's `parseViewport` accepts `<W>x<H>`.
    // Always set; the BrowserConfig defaults guarantee non-zero
    // values (1920×1080) and the operator can override either
    // dimension independently in `aura.json`.
    extra_env.insert("AURA_BROWSER_VIEWPORT".into(), format!("{width}x{height}"));
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
    // are not actionable for Aura operators anyway.
    extra_env.insert("NODE_OPTIONS".into(), "--no-warnings".into());
    if !extra_font_dirs.is_empty() {
        let joined = extra_font_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        extra_env.insert("AURA_BROWSER_EXTRA_FONT_DIRS".into(), joined);
    }
    // Docker mode plumbing. All four are presence-only / value-bearing
    // env vars the TS wrapper reads at startup. `cdp_url` takes
    // precedence: when set, the wrapper skips docker entirely and
    // connects to the operator's Chrome.
    if docker_enable {
        extra_env.insert("AURA_BROWSER_DOCKER_ENABLE".into(), "1".into());
    }
    if let Some(url) = docker_cdp_url {
        extra_env.insert("AURA_BROWSER_DOCKER_CDP_URL".into(), url.into());
    }
    if let Some(port) = docker_web_vnc_port {
        extra_env.insert("AURA_BROWSER_DOCKER_WEB_VNC_PORT".into(), port.to_string());
    }
    if let Some(tag) = docker_image_tag {
        extra_env.insert("AURA_BROWSER_DOCKER_IMAGE_TAG".into(), tag.into());
    }
    Some(EmbeddedMcpProfile {
        server_name: "browser".into(),
        command,
        args: vec![bundle_path.display().to_string()],
        // Empty by design — see the doc comment above. With no
        // capability ceiling and no per-tool `_meta.aura.access_rule`
        // (CDDM doesn't emit any), the McpTool wrapper's
        // `accessed_resources()` returns `[]` and the agent loop
        // never prompts on browser tool calls.
        capabilities: Vec::new(),
        extra_env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `sandbox=false` matches the `BrowserConfig` default and exercises
    /// the AURA_BROWSER_NO_SANDBOX-on path; the override case is
    /// covered by `sandbox_on_clears_no_sandbox_env` below.
    fn defaults_call(enable: bool) -> Option<EmbeddedMcpProfile> {
        browser_mcp_profile(
            enable,
            None,
            None,
            false, // sandbox
            1920,
            1080,
            "node".into(),
            Path::new("/x.mjs"),
            &[],
            false, // docker_enable
            None,  // docker_cdp_url
            None,  // docker_web_vnc_port
            None,  // docker_image_tag
        )
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
            p.extra_env.get("AURA_BROWSER_NO_SANDBOX"),
            Some(&"1".to_string()),
            "sandbox=false default → AURA_BROWSER_NO_SANDBOX=1 so the TS sidecar appends --no-sandbox",
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
    /// loop's pre-execute approval prompt. This is enforced by
    /// `capabilities: vec![]` here (no default ResourceAccess) plus
    /// CDDM emitting no `_meta.aura.access_rule` annotations. If a
    /// future refactor sneaks Http or ExecCommand into the browser
    /// profile's capability list, the McpTool wrapper's
    /// `accessed_resources()` would return non-empty and every
    /// `browser/*` tool call would start prompting — silently
    /// degrading UX. This test fails loud if that happens.
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
        let p = browser_mcp_profile(
            true,
            None,
            None,
            true, // sandbox
            1920,
            1080,
            "node".into(),
            Path::new("/x.mjs"),
            &[],
            false,
            None,
            None,
            None,
        )
        .expect("profile when enabled");
        assert!(
            !p.extra_env.contains_key("AURA_BROWSER_NO_SANDBOX"),
            "sandbox=true must NOT set AURA_BROWSER_NO_SANDBOX so Chrome launches with its sandbox on",
        );
    }

    #[test]
    fn chrome_path_lands_in_env() {
        let p = browser_mcp_profile(
            true,
            Some(&PathBuf::from("/opt/google/chrome/chrome")),
            None,
            false,
            1920,
            1080,
            "node".into(),
            Path::new("/x.mjs"),
            &[],
            false,
            None,
            None,
            None,
        )
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_BROWSER_CHROME_PATH").unwrap(),
            "/opt/google/chrome/chrome"
        );
    }

    #[test]
    fn profile_dir_lands_in_env() {
        let p = browser_mcp_profile(
            true,
            None,
            Some(&PathBuf::from("/var/aura/browser-profile")),
            false,
            1920,
            1080,
            "node".into(),
            Path::new("/x.mjs"),
            &[],
            false,
            None,
            None,
            None,
        )
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_BROWSER_PROFILE_DIR").unwrap(),
            "/var/aura/browser-profile"
        );
    }

    #[test]
    fn viewport_lands_in_env_as_wxh() {
        let p = browser_mcp_profile(
            true,
            None,
            None,
            false,
            1280,
            720,
            "node".into(),
            Path::new("/x.mjs"),
            &[],
            false,
            None,
            None,
            None,
        )
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_BROWSER_VIEWPORT").unwrap(),
            "1280x720",
            "viewport encoded as <W>x<H> for the TS wrapper's parseViewport",
        );
    }

    #[test]
    fn viewport_default_lands_in_env() {
        let p = defaults_call(true).expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_BROWSER_VIEWPORT").unwrap(),
            "1920x1080",
        );
    }

    #[test]
    fn empty_font_dirs_omits_env() {
        let p = defaults_call(true).expect("profile when enabled");
        assert!(
            !p.extra_env.contains_key("AURA_BROWSER_EXTRA_FONT_DIRS"),
            "empty extra_font_dirs must NOT set the env var so the TS sidecar skips fontconfig override entirely",
        );
    }

    #[test]
    fn font_dirs_join_with_colon() {
        let a = PathBuf::from("/work/.fonts");
        let b = PathBuf::from("/usr/share/extra-fonts");
        let p = browser_mcp_profile(
            true,
            None,
            None,
            false,
            1920,
            1080,
            "node".into(),
            Path::new("/x.mjs"),
            &[a.as_path(), b.as_path()],
            false,
            None,
            None,
            None,
        )
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_BROWSER_EXTRA_FONT_DIRS").unwrap(),
            "/work/.fonts:/usr/share/extra-fonts",
            "multiple font dirs encoded as colon-joined path list (PATH-style) for the TS wrapper",
        );
    }

    #[test]
    fn docker_disabled_omits_all_docker_env() {
        let p = defaults_call(true).expect("profile when enabled");
        for key in [
            "AURA_BROWSER_DOCKER_ENABLE",
            "AURA_BROWSER_DOCKER_CDP_URL",
            "AURA_BROWSER_DOCKER_WEB_VNC_PORT",
            "AURA_BROWSER_DOCKER_IMAGE_TAG",
        ] {
            assert!(
                !p.extra_env.contains_key(key),
                "docker mode off must omit {key} so the TS wrapper takes the host-headless path",
            );
        }
    }

    #[test]
    fn docker_enable_lands_in_env() {
        let p = browser_mcp_profile(
            true,
            None,
            None,
            false,
            1920,
            1080,
            "node".into(),
            Path::new("/x.mjs"),
            &[],
            true,
            None,
            None,
            None,
        )
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_BROWSER_DOCKER_ENABLE"),
            Some(&"1".to_string()),
            "docker_enable=true → AURA_BROWSER_DOCKER_ENABLE=1 (presence-only flag)",
        );
    }

    #[test]
    fn docker_cdp_url_lands_in_env() {
        let p = browser_mcp_profile(
            true,
            None,
            None,
            false,
            1920,
            1080,
            "node".into(),
            Path::new("/x.mjs"),
            &[],
            false,
            Some("http://10.0.0.5:9222"),
            None,
            None,
        )
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_BROWSER_DOCKER_CDP_URL").unwrap(),
            "http://10.0.0.5:9222",
        );
    }

    #[test]
    fn docker_image_tag_override_lands_in_env() {
        let p = browser_mcp_profile(
            true,
            None,
            None,
            false,
            1920,
            1080,
            "node".into(),
            Path::new("/x.mjs"),
            &[],
            true,
            None,
            None,
            Some("my-registry/aura-browser:pinned"),
        )
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_BROWSER_DOCKER_IMAGE_TAG").unwrap(),
            "my-registry/aura-browser:pinned",
            "operator-supplied tag short-circuits the deterministic tag computation in the wrapper",
        );
    }

    #[test]
    fn docker_web_vnc_port_lands_in_env_as_string() {
        let p = browser_mcp_profile(
            true,
            None,
            None,
            false,
            1920,
            1080,
            "node".into(),
            Path::new("/x.mjs"),
            &[],
            true,
            None,
            Some(6080),
            None,
        )
        .expect("profile when enabled");
        assert_eq!(
            p.extra_env.get("AURA_BROWSER_DOCKER_WEB_VNC_PORT").unwrap(),
            "6080",
            "web_vnc_port serialised to a decimal string for the TS wrapper to parse",
        );
    }
}
