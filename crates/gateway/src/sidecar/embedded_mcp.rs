//! Gateway-side glue for embedded MCP servers.
//!
//! The protocol-policy types (`EmbeddedMcpProfile`, `browser_mcp_profile`,
//! `embedded_servers`) live in [`baybo_tools::mcp::profile`]. This
//! module owns the host-tool integration: locating the host's `node`
//! binary (via [`baybo_process::HostTool`]), which materialised bundle
//! path each profile gets, and the per-domain composition that turns
//! the operator's [`BayboConfig`] into the profile list the reconciler
//! consumes.

use baybo_config::BayboConfig;
use baybo_tools::mcp::{BrowserProfileParams, EmbeddedMcpProfile, browser_mcp_profile};
use baybo_workspace::WorkspacePaths;

use crate::sidecar::SidecarRuntime;

/// Walk every tool-domain family and collect the [`EmbeddedMcpProfile`]
/// list to hand to [`baybo_tools::mcp::embedded_servers`].
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
    config: &BayboConfig,
    workspace_paths: &WorkspacePaths,
) -> Vec<EmbeddedMcpProfile> {
    let node_cmd = baybo_process::HostTool::node().path().display().to_string();
    let browser_font_dir = workspace_paths.browser_fonts_dir();
    // Pin the Chrome profile under the workspace by default so it sits
    // next to other workspace-scoped state (logs, uv cache, …) and
    // follows the operator's `workspace.path` rather than living in
    // `$XDG_CACHE_HOME`. Operator override (`browser.profile_dir`) still
    // wins when set.
    let workspace_browser_profile = workspace_paths.browser_profile_dir();
    let effective_profile_dir = config
        .browser
        .profile_dir
        .as_deref()
        .unwrap_or(workspace_browser_profile.as_path());
    // Read-only view of the agent's own work dir, so a `file://` URL for
    // an artefact the agent just wrote resolves inside the container.
    // Opt-out leaves the container with no view of the workspace at all.
    let browser_work_dir = workspace_paths.work_dir();
    [runtime.bundle_for("browser").and_then(|bundle| {
        browser_mcp_profile(BrowserProfileParams {
            enable: config.browser.enable,
            chrome_path: config.browser.chrome_path.as_deref(),
            profile_dir: Some(effective_profile_dir),
            sandbox: config.browser.sandbox,
            width: config.browser.width,
            height: config.browser.height,
            command: node_cmd.clone(),
            bundle_path: bundle,
            extra_font_dirs: &[browser_font_dir.as_path()],
            docker_enable: config.browser.docker.enable,
            docker_cdp_url: config.browser.docker.cdp_url.as_deref(),
            docker_web_vnc_port: config.browser.docker.web_vnc_port,
            docker_image_tag: config.browser.docker.image_tag.as_deref(),
            docker_work_dir: config
                .browser
                .docker
                .mount_work_dir
                .then_some(browser_work_dir.as_path()),
            docker_memory_limit_mb: config.browser.docker.memory_limit_mb,
        })
    })]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use baybo_config::BayboConfig;

    fn ws() -> WorkspacePaths {
        WorkspacePaths::new(PathBuf::from("/tmp/baybo-test-workspace"))
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
        let cfg = BayboConfig::default();
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
        let mut cfg = BayboConfig::default();
        cfg.browser.enable = true;
        let profiles = collect_profiles(&rt, &cfg, &ws());
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].server_name, "browser");
        let font_dirs = profiles[0]
            .extra_env
            .get("BAYBO_BROWSER_EXTRA_FONT_DIRS")
            .expect("collect_profiles must pin <work>/.fonts as a Chrome fontconfig dir");
        assert_eq!(font_dirs, "/tmp/baybo-test-workspace/work/.fonts");
        let profile_dir = profiles[0]
            .extra_env
            .get("BAYBO_BROWSER_PROFILE_DIR")
            .expect("collect_profiles must default the Chrome profile to <state>/browser/profile");
        assert_eq!(
            profile_dir,
            "/tmp/baybo-test-workspace/state/browser/profile"
        );
        assert!(
            !profiles[0]
                .extra_env
                .contains_key("BAYBO_BROWSER_DOCKER_ENABLE"),
            "default browser config must not turn docker mode on",
        );
    }

    /// Operator override (`browser.profile_dir = "..."`) wins over the
    /// workspace default. Catches a reordering bug that would silently
    /// keep the workspace path even when the operator pinned a custom
    /// location (a regression that wouldn't be visible until cookies
    /// went missing).
    #[test]
    fn operator_profile_dir_override_wins_over_workspace_default() {
        let Ok(rt) = SidecarRuntime::install() else {
            return;
        };
        if rt.bundle_for("browser").is_none() {
            return;
        }
        let mut cfg = BayboConfig::default();
        cfg.browser.enable = true;
        cfg.browser.profile_dir = Some(PathBuf::from("/var/baybo/explicit-profile"));
        let profiles = collect_profiles(&rt, &cfg, &ws());
        assert_eq!(
            profiles[0]
                .extra_env
                .get("BAYBO_BROWSER_PROFILE_DIR")
                .unwrap(),
            "/var/baybo/explicit-profile",
            "explicit baybo.json:browser.profile_dir must trump the workspace default",
        );
    }

    /// Brackets the docker-substruct wiring: `cfg.browser.docker.*`
    /// fields must reach the child process as `BAYBO_BROWSER_DOCKER_*`
    /// env vars. Catches a typo on the substruct path that would
    /// silently drop docker mode at the boundary.
    #[test]
    fn docker_mode_propagates() {
        let Ok(rt) = SidecarRuntime::install() else {
            return;
        };
        if rt.bundle_for("browser").is_none() {
            return;
        }
        let mut cfg = BayboConfig::default();
        cfg.browser.enable = true;
        cfg.browser.docker.enable = true;
        cfg.browser.docker.web_vnc_port = Some(6080);
        cfg.browser.docker.image_tag = Some("custom/chrome:test".into());
        let profiles = collect_profiles(&rt, &cfg, &ws());
        assert_eq!(profiles.len(), 1);
        let env = &profiles[0].extra_env;
        assert_eq!(
            env.get("BAYBO_BROWSER_DOCKER_ENABLE"),
            Some(&"1".to_string())
        );
        assert_eq!(
            env.get("BAYBO_BROWSER_DOCKER_WEB_VNC_PORT"),
            Some(&"6080".to_string())
        );
        assert_eq!(
            env.get("BAYBO_BROWSER_DOCKER_IMAGE_TAG"),
            Some(&"custom/chrome:test".to_string())
        );
    }

    /// The work mount is what makes `file://` URLs for agent-written
    /// artefacts resolve inside the container. It has to point at the
    /// workspace's `work/` dir specifically — the fonts dir sits one
    /// level below it, so a wrong accessor here still looks plausible.
    #[test]
    fn work_dir_is_mounted_by_default_and_points_at_the_work_root() {
        let Ok(rt) = SidecarRuntime::install() else {
            return;
        };
        if rt.bundle_for("browser").is_none() {
            return;
        }
        let mut cfg = BayboConfig::default();
        cfg.browser.enable = true;
        cfg.browser.docker.enable = true;
        let profiles = collect_profiles(&rt, &cfg, &ws());
        assert_eq!(
            profiles[0]
                .extra_env
                .get("BAYBO_BROWSER_DOCKER_WORK_DIR")
                .expect("work dir is mounted by default"),
            "/tmp/baybo-test-workspace/work",
        );
    }

    #[test]
    fn mount_work_dir_false_drops_the_mount_entirely() {
        let Ok(rt) = SidecarRuntime::install() else {
            return;
        };
        if rt.bundle_for("browser").is_none() {
            return;
        }
        let mut cfg = BayboConfig::default();
        cfg.browser.enable = true;
        cfg.browser.docker.enable = true;
        cfg.browser.docker.mount_work_dir = false;
        let profiles = collect_profiles(&rt, &cfg, &ws());
        assert!(
            !profiles[0]
                .extra_env
                .contains_key("BAYBO_BROWSER_DOCKER_WORK_DIR"),
            "opting out must omit the var, not pass an empty path the wrapper would try to bind",
        );
    }

    /// The memory ceiling defaults to a real value, so a wiring slip that
    /// dropped it would leave the deployment uncapped while the config
    /// still read as capped.
    #[test]
    fn memory_ceiling_reaches_the_child_by_default() {
        let Ok(rt) = SidecarRuntime::install() else {
            return;
        };
        if rt.bundle_for("browser").is_none() {
            return;
        }
        let mut cfg = BayboConfig::default();
        cfg.browser.enable = true;
        cfg.browser.docker.enable = true;
        let profiles = collect_profiles(&rt, &cfg, &ws());
        let env = &profiles[0].extra_env;
        assert_eq!(
            env.get("BAYBO_BROWSER_DOCKER_MEMORY_LIMIT"),
            Some(&"4096m".to_string()),
        );
    }
}
