use std::path::PathBuf;
use std::process::Command;

use crate::installer::{InstallContext, InstallerError, Result, ServiceInstaller, ServiceStatus};

const LABEL: &str = "com.baybo.gateway";

pub struct LaunchdInstaller;

impl Default for LaunchdInstaller {
    fn default() -> Self {
        Self::new()
    }
}

impl LaunchdInstaller {
    pub fn new() -> Self {
        Self
    }

    fn plist_dir(&self) -> Result<PathBuf> {
        let home = std::env::var_os("HOME").ok_or(InstallerError::NoHome)?;
        Ok(PathBuf::from(home).join("Library/LaunchAgents"))
    }

    fn plist_path(&self) -> PathBuf {
        self.plist_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(format!("{LABEL}.plist"))
    }

    fn launchctl(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("launchctl").args(args).output().map_err(|e| {
            InstallerError::External {
                cmd: format!("launchctl {}", args.join(" ")),
                status: "exec".into(),
                stderr: e.to_string(),
            }
        })?;
        if !out.status.success() {
            return Err(InstallerError::External {
                cmd: format!("launchctl {}", args.join(" ")),
                status: out.status.code().map_or("?".into(), |c| c.to_string()),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn service_target(&self) -> String {
        // Safety: `getuid` cannot fail per POSIX.
        let uid = unsafe { libc::getuid() };
        format!("gui/{uid}/{LABEL}")
    }
}

impl ServiceInstaller for LaunchdInstaller {
    fn unit_path(&self) -> PathBuf {
        self.plist_path()
    }

    fn render_unit(&self, ctx: &InstallContext) -> String {
        let exec = xml_escape(&ctx.exec_start.display().to_string());
        let log = xml_escape(&ctx.log_dir.display().to_string());
        // Always emitted now: a launchd agent's default `PATH` is
        // narrower than systemd's (no `/usr/local/bin`, so not even
        // Homebrew), which leaves every host tool baybo shells out to
        // unresolvable. See `resolve_service_path`.
        let mut env_block = String::from("    <key>EnvironmentVariables</key>\n    <dict>\n");
        env_block.push_str("      <key>PATH</key>\n");
        env_block.push_str(&format!(
            "      <string>{}</string>\n",
            xml_escape(&ctx.path_env)
        ));
        if let Some(cfg) = &ctx.config_path {
            env_block.push_str(&format!(
                "      <key>{}</key>\n",
                baybo_workspace::paths::ENV_CONFIG_PATH
            ));
            env_block.push_str(&format!(
                "      <string>{}</string>\n",
                xml_escape(&cfg.display().to_string())
            ));
        }
        env_block.push_str("    </dict>\n");
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
      <string>{exec}</string>
      <string>gateway</string>
      <string>start</string>
    </array>
    <key>RunAtLoad</key>
    <false/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>2</integer>
    <key>StandardOutPath</key>
    <string>{log}/baybo-gateway.out.log</string>
    <key>StandardErrorPath</key>
    <string>{log}/baybo-gateway.err.log</string>
{env_block}  </dict>
</plist>
"#,
            label = LABEL,
        )
    }

    fn install(&self, ctx: &InstallContext) -> Result<PathBuf> {
        let dir = self.plist_dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| InstallerError::Io {
            path: dir.display().to_string(),
            reason: e.to_string(),
        })?;
        let path = dir.join(format!("{LABEL}.plist"));
        let body = self.render_unit(ctx);
        std::fs::write(&path, body).map_err(|e| InstallerError::Io {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Ok(path)
    }

    fn enable(&self) -> Result<()> {
        let path = self.plist_path();
        self.launchctl(&["load", "-w", path.to_str().unwrap_or_default()])?;
        self.launchctl(&["kickstart", "-k", &self.service_target()])?;
        Ok(())
    }

    fn restart(&self) -> Result<()> {
        self.launchctl(&["kickstart", "-k", &self.service_target()])?;
        Ok(())
    }

    fn disable(&self) -> Result<()> {
        let path = self.plist_path();
        self.launchctl(&["unload", "-w", path.to_str().unwrap_or_default()])?;
        Ok(())
    }

    fn uninstall(&self) -> Result<()> {
        let path = self.plist_path();
        // Best-effort unload.
        let _ = self.launchctl(&["unload", "-w", path.to_str().unwrap_or_default()]);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| InstallerError::Io {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        }
        Ok(())
    }

    fn status(&self) -> Result<ServiceStatus> {
        let path = self.plist_path();
        if !path.exists() {
            return Ok(ServiceStatus::NotInstalled);
        }
        let list = self.launchctl(&["list"]).unwrap_or_default();
        if list.contains(LABEL) {
            Ok(ServiceStatus::Running)
        } else {
            Ok(ServiceStatus::Installed)
        }
    }
}

/// Minimal XML text escaping for the values interpolated into the
/// plist. A `PATH` is a concatenation of arbitrary operator directory
/// names, so an unescaped `&` in one of them would render a plist
/// `launchctl` refuses to load — with no hint as to why.
fn xml_escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> InstallContext {
        InstallContext {
            exec_start: PathBuf::from("/usr/local/bin/baybo"),
            config_path: Some(PathBuf::from("/Users/me/.baybo/config/baybo.json")),
            log_dir: PathBuf::from("/Users/me/.baybo/logs"),
            path_env: "/opt/homebrew/bin:/usr/local/bin:/usr/bin".to_string(),
            run_as: None,
        }
    }

    #[test]
    fn render_plist_has_program_arguments_and_env() {
        let inst = LaunchdInstaller::new();
        let body = inst.render_unit(&ctx());
        assert!(body.contains("<string>com.baybo.gateway</string>"));
        assert!(body.contains("<string>/usr/local/bin/baybo</string>"));
        assert!(body.contains("<string>gateway</string>"));
        assert!(body.contains("BAYBO_CONFIG_PATH"));
        assert!(body.contains("/Users/me/.baybo/config/baybo.json"));
        assert!(body.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn render_plist_without_config_path() {
        let inst = LaunchdInstaller::new();
        let mut c = ctx();
        c.config_path = None;
        let body = inst.render_unit(&c);
        assert!(!body.contains("BAYBO_CONFIG_PATH"));
    }
}
