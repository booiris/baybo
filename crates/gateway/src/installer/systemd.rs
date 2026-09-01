use std::path::PathBuf;
use std::process::Command;

use crate::installer::{InstallContext, InstallerError, Result, ServiceInstaller, ServiceStatus};

const UNIT_NAME: &str = "baybo-gateway.service";

pub struct SystemdInstaller {
    user_mode: bool,
}

impl SystemdInstaller {
    pub fn new(user_mode: bool) -> Self {
        Self { user_mode }
    }

    fn unit_dir(&self) -> Result<PathBuf> {
        if self.user_mode {
            let home = std::env::var_os("HOME").ok_or(InstallerError::NoHome)?;
            Ok(PathBuf::from(home).join(".config/systemd/user"))
        } else {
            Ok(PathBuf::from("/etc/systemd/system"))
        }
    }

    fn systemctl(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("systemctl");
        if self.user_mode {
            cmd.arg("--user");
        }
        cmd.args(args);
        let out = cmd.output().map_err(|e| InstallerError::External {
            cmd: format!("systemctl {}", args.join(" ")),
            status: "exec".into(),
            stderr: e.to_string(),
        })?;
        if !out.status.success() {
            return Err(InstallerError::External {
                cmd: format!("systemctl {}", args.join(" ")),
                status: out.status.code().map_or("?".into(), |c| c.to_string()),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl ServiceInstaller for SystemdInstaller {
    fn unit_path(&self) -> PathBuf {
        self.unit_dir()
            .unwrap_or_else(|_| PathBuf::from("/etc/systemd/system"))
            .join(UNIT_NAME)
    }

    fn render_unit(&self, ctx: &InstallContext) -> String {
        let mut out = String::new();
        out.push_str("[Unit]\n");
        out.push_str("Description=Baybo HTTP gateway\n");
        out.push_str("After=network-online.target\n");
        out.push_str("Wants=network-online.target\n\n");

        out.push_str("[Service]\n");
        out.push_str("Type=simple\n");
        out.push_str(&format!(
            "ExecStart={} gateway start\n",
            ctx.exec_start.display()
        ));
        // System-wide unit: drop to the operator's account. Absent this,
        // systemd runs the daemon as root against a workspace owned by
        // whoever ran the install. `Group=` is left out on purpose —
        // systemd defaults it to the user's primary group, which is
        // what we want and cannot get wrong.
        if let Some(user) = &ctx.run_as {
            out.push_str(&format!("User={}\n", user.as_str()));
        }
        out.push_str("Restart=on-failure\n");
        out.push_str("RestartSec=2\n");
        out.push_str("TimeoutStopSec=30\n");
        // Without this the unit inherits the user manager's default
        // (`/usr/local/bin:/usr/bin`) and every host tool installed under
        // $HOME — bun, uv, the external-agent CLIs — becomes invisible to
        // a daemon whose operator can run all of them from their shell.
        out.push_str(&format!("Environment=PATH={}\n", ctx.path_env));
        if let Some(cfg) = &ctx.config_path {
            out.push_str(&format!(
                "Environment={}={}\n",
                baybo_workspace::paths::ENV_CONFIG_PATH,
                cfg.display()
            ));
        }
        out.push_str(&format!(
            "# Log directory (ensure it exists and is writable): {}\n",
            ctx.log_dir.display()
        ));
        out.push('\n');

        out.push_str("[Install]\n");
        out.push_str("WantedBy=default.target\n");
        out
    }

    fn install(&self, ctx: &InstallContext) -> Result<PathBuf> {
        let dir = self.unit_dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| InstallerError::Io {
            path: dir.display().to_string(),
            reason: e.to_string(),
        })?;
        let path = dir.join(UNIT_NAME);
        let body = self.render_unit(ctx);
        std::fs::write(&path, body).map_err(|e| InstallerError::Io {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let _ = self.systemctl(&["daemon-reload"]);
        Ok(path)
    }

    fn enable(&self) -> Result<()> {
        self.systemctl(&["enable", "--now", UNIT_NAME])?;
        Ok(())
    }

    fn restart(&self) -> Result<()> {
        self.systemctl(&["restart", UNIT_NAME])?;
        Ok(())
    }

    fn disable(&self) -> Result<()> {
        self.systemctl(&["disable", "--now", UNIT_NAME])?;
        Ok(())
    }

    fn uninstall(&self) -> Result<()> {
        // Best-effort disable; don't fail if the unit was already disabled.
        let _ = self.systemctl(&["disable", UNIT_NAME]);
        let _ = self.systemctl(&["stop", UNIT_NAME]);
        let path = self.unit_path();
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| InstallerError::Io {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?;
        }
        let _ = self.systemctl(&["daemon-reload"]);
        Ok(())
    }

    fn status(&self) -> Result<ServiceStatus> {
        if !self.unit_path().exists() {
            return Ok(ServiceStatus::NotInstalled);
        }
        let is_active = self.systemctl(&["is-active", UNIT_NAME]).is_ok();
        let is_enabled = self.systemctl(&["is-enabled", UNIT_NAME]).is_ok();
        Ok(match (is_enabled, is_active) {
            (_, true) => ServiceStatus::Running,
            (true, false) => ServiceStatus::Enabled,
            (false, false) => ServiceStatus::Installed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> InstallContext {
        InstallContext {
            exec_start: PathBuf::from("/usr/local/bin/baybo"),
            config_path: Some(PathBuf::from("/home/user/.baybo/config/baybo.json")),
            log_dir: PathBuf::from("/home/user/.baybo/logs"),
            path_env: "/home/user/.local/bin:/usr/local/bin:/usr/bin".to_string(),
            run_as: None,
        }
    }

    #[test]
    fn render_unit_has_exec_start_and_config_env() {
        let inst = SystemdInstaller::new(true);
        let body = inst.render_unit(&ctx());
        assert!(body.contains("ExecStart=/usr/local/bin/baybo gateway start"));
        assert!(body.contains("Environment=BAYBO_CONFIG_PATH=/home/user/.baybo/config/baybo.json"));
        assert!(body.contains("Restart=on-failure"));
        assert!(body.contains("RestartSec=2"));
    }

    /// The unit must pin `PATH`. Without it systemd hands the daemon
    /// `/usr/local/bin:/usr/bin` and every `$HOME`-installed host tool
    /// (bun, uv, the external-agent CLIs) goes missing — the deck
    /// services and channel sidecars die on ENOENT while the operator's
    /// own shell runs the same binaries fine.
    #[test]
    fn render_unit_pins_path() {
        let inst = SystemdInstaller::new(true);
        let body = inst.render_unit(&ctx());
        assert!(
            body.contains("Environment=PATH=/home/user/.local/bin:/usr/local/bin:/usr/bin"),
            "unit must carry the resolved service PATH, got:\n{body}"
        );
    }

    /// A per-user unit must NOT name an account — the user manager
    /// already runs as the user, and a stray `User=` there is an error.
    #[test]
    fn user_unit_names_no_account() {
        let inst = SystemdInstaller::new(true);
        let body = inst.render_unit(&ctx());
        assert!(
            !body.contains("User="),
            "a per-user unit must not carry User=, got:\n{body}"
        );
    }

    /// A system-wide unit must drop to the operator's account. With no
    /// `User=` systemd runs the daemon as root against a workspace owned
    /// by whoever ran the install, leaving root-owned `storage.db`,
    /// vault and `personas/` that the user's own CLI can no longer
    /// write. `Group=` is intentionally absent: systemd defaults it to
    /// the user's primary group.
    #[test]
    fn system_unit_drops_to_the_resolved_account() {
        let inst = SystemdInstaller::new(false);
        let mut c = ctx();
        c.run_as = Some(crate::installer::resolve_service_user(Some("booiris")).expect("resolve"));
        let body = inst.render_unit(&c);
        assert!(
            body.contains("\nUser=booiris\n"),
            "system unit must drop to the resolved account, got:\n{body}"
        );
        assert!(
            !body.contains("Group="),
            "Group= is left to systemd's default (the user's primary group), got:\n{body}"
        );
    }

    /// `PATH` is not optional the way the config path is: a unit with no
    /// config still boots on defaults, a unit with no `PATH` cannot run
    /// a single sidecar.
    #[test]
    fn render_unit_without_config_path() {
        let inst = SystemdInstaller::new(true);
        let mut c = ctx();
        c.config_path = None;
        let body = inst.render_unit(&c);
        assert!(!body.contains("BAYBO_CONFIG_PATH"));
        assert!(body.contains("Environment=PATH="));
    }
}
