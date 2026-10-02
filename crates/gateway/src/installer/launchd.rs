use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::installer::{
    InstallContext, InstallerError, Result, START_SETTLE, ServiceInstaller, ServiceStatus,
};

const LABEL: &str = "com.baybo.gateway";
/// The one-shot job `preflight` bootstraps. Its own label, so probing never
/// disturbs an installed gateway.
const PREFLIGHT_LABEL: &str = "com.baybo.gateway.preflight";
/// Service logs live on the boot volume, under the user's own Logs folder.
const SERVICE_LOG_SUBDIR: &str = "Library/Logs/baybo";
/// What launchd records when it could not even set the job up — its
/// `EX_CONFIG`. It never reaches our binary, so baybo cannot log anything.
const LAUNCHD_EX_CONFIG: i32 = 78;
const PREFLIGHT_POLL: Duration = Duration::from_millis(250);
/// The OS's own wording for a TCC denial, which is what tells a privacy
/// block apart from every other way the probe can fail.
const TCC_DENIAL: &str = "Operation not permitted";
const FULL_DISK_ACCESS_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

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

    fn home(&self) -> Result<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(InstallerError::NoHome)
    }

    fn plist_dir(&self) -> Result<PathBuf> {
        Ok(self.home()?.join("Library/LaunchAgents"))
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

    fn domain(&self) -> String {
        // Safety: `getuid` cannot fail per POSIX.
        let uid = unsafe { libc::getuid() };
        format!("gui/{uid}")
    }

    fn service_target(&self) -> String {
        format!("{}/{LABEL}", self.domain())
    }

    /// `None` when the job is not loaded at all.
    fn job_state(&self, label: &str) -> Option<JobState> {
        self.launchctl(&["print", &format!("{}/{label}", self.domain())])
            .ok()
            .map(|out| JobState::parse(&out))
    }

    /// Bootstrap the one-shot probe, wait for it to exit, and hand back its
    /// exit code and combined output. Always unloads the probe again.
    fn run_preflight_job(
        &self,
        ctx: &InstallContext,
        dir: &Path,
        wait: Duration,
    ) -> Result<(i32, String)> {
        let out_log = dir.join("preflight.out.log");
        let err_log = dir.join("preflight.err.log");
        let plist = dir.join(format!("{PREFLIGHT_LABEL}.plist"));
        let body = render_agent(&AgentPlist {
            label: PREFLIGHT_LABEL,
            exec: &ctx.exec_start,
            args: &["gateway", "preflight"],
            stdout: &out_log,
            stderr: &err_log,
            ctx,
            supervised: false,
        });
        write_file(&plist, &body)?;

        let target = format!("{}/{PREFLIGHT_LABEL}", self.domain());
        // A probe left behind by an interrupted run would make bootstrap fail.
        let _ = self.launchctl(&["bootout", &target]);
        self.launchctl(&["bootstrap", &self.domain(), &plist.display().to_string()])?;

        let deadline = Instant::now() + wait;
        let exit = loop {
            if let Some(JobState {
                pid: None,
                last_exit: Some(code),
                ..
            }) = self.job_state(PREFLIGHT_LABEL)
            {
                break Some(code);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(PREFLIGHT_POLL);
        };
        let _ = self.launchctl(&["bootout", &target]);

        let output = [&err_log, &out_log]
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .collect::<Vec<_>>()
            .join("\n");
        // Not a crash and not a refusal: the probe is parked inside `open()`.
        // That is what a TCC decision nobody has made yet looks like — the
        // kernel holds the call until the operator answers the dialog.
        let exit = exit.ok_or_else(|| InstallerError::Preflight {
            detail: format!(
                "the gateway was still waiting to open a file after {}s",
                wait.as_secs()
            ),
            remedy: Some(pending_prompt_remedy(&ctx.exec_start)),
        })?;
        Ok((exit, output.trim().to_string()))
    }
}

impl ServiceInstaller for LaunchdInstaller {
    fn unit_path(&self) -> PathBuf {
        self.plist_path()
    }

    fn render_unit(&self, ctx: &InstallContext) -> String {
        let out_log = ctx.log_dir.join("baybo-gateway.out.log");
        let err_log = ctx.log_dir.join("baybo-gateway.err.log");
        render_agent(&AgentPlist {
            label: LABEL,
            exec: &ctx.exec_start,
            args: &["gateway", "start"],
            stdout: &out_log,
            stderr: &err_log,
            ctx,
            supervised: true,
        })
    }

    fn install(&self, ctx: &InstallContext) -> Result<PathBuf> {
        let dir = self.plist_dir()?;
        create_dir(&dir)?;
        // launchd opens these before exec'ing us and gives up on the job
        // if it cannot — it will not create the directory itself.
        create_dir(&ctx.log_dir)?;
        let path = dir.join(format!("{LABEL}.plist"));
        write_file(&path, &self.render_unit(ctx))?;
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
        if !self.plist_path().exists() {
            return Ok(ServiceStatus::NotInstalled);
        }
        Ok(self
            .job_state(LABEL)
            .map_or(ServiceStatus::Installed, |s| s.status()))
    }

    /// `~/Library/Logs/baybo`, never the workspace. launchd opens a job's
    /// `StandardOutPath` / `StandardErrorPath` ITSELF, before exec'ing the
    /// binary, and a background job may not touch an external volume (or
    /// `~/Documents`, `~/Desktop`, iCloud, a network share) without a TCC
    /// grant. With the workspace on such a volume the job died with
    /// `EX_CONFIG` before baybo ran, leaving no log line anywhere — and a
    /// Full Disk Access grant to baybo does not help, because it is launchd
    /// opening the file, not baybo.
    fn log_dir(&self, workspace_logs: &Path) -> PathBuf {
        self.home().map_or_else(
            |_| workspace_logs.to_path_buf(),
            |home| home.join(SERVICE_LOG_SUBDIR),
        )
    }

    fn logs_hint(&self, log_dir: &Path) -> String {
        log_dir.join("baybo-gateway.err.log").display().to_string()
    }

    fn preflight(&self, ctx: &InstallContext, wait: Duration) -> Result<()> {
        let dir = std::env::temp_dir().join(format!("baybo-preflight-{}", std::process::id()));
        create_dir(&dir)?;
        let result = self.run_preflight_job(ctx, &dir, wait);
        let _ = std::fs::remove_dir_all(&dir);
        let (exit, output) = result?;
        if exit == 0 {
            return Ok(());
        }
        let remedy = output
            .contains(TCC_DENIAL)
            .then(|| denied_remedy(&ctx.exec_start, &output));
        Err(InstallerError::Preflight {
            detail: format!(
                "launchd could not run the gateway (preflight exited {exit}):\n{output}"
            ),
            remedy,
        })
    }

    fn preflight_notice(&self) -> Option<&'static str> {
        Some(
            "Checking the gateway can reach your workspace as a background service. \
             If macOS asks whether baybo may access files on a volume or in a folder, \
             click Allow.",
        )
    }

    fn open_access_settings(&self, ctx: &InstallContext) -> bool {
        let opened = Command::new("open")
            .arg(FULL_DISK_ACCESS_SETTINGS)
            .status()
            .is_ok_and(|s| s.success());
        // Selected in Finder, so it can be dragged straight into the list.
        let _ = Command::new("open").arg("-R").arg(&ctx.exec_start).status();
        opened
    }

    /// Judged by launchd's spawn counter, not by a single pid sample: a job
    /// in a crash loop briefly HAS a pid on every respawn, so "is there a pid
    /// right now" can say yes to a gateway that never stays up. Any respawn
    /// inside the settle window means it is not staying up.
    fn confirm_started(&self) -> Result<()> {
        let before = self.job_state(LABEL);
        std::thread::sleep(START_SETTLE);
        let after = self
            .job_state(LABEL)
            .ok_or_else(|| InstallerError::Other("the service is not loaded in launchd".into()))?;
        let respawned = matches!(
            (before.and_then(|b| b.runs), after.runs),
            (Some(b), Some(a)) if a > b
        );
        if after.pid.is_some() && !respawned {
            return Ok(());
        }
        let last_exit = after.last_exit.unwrap_or_default();
        let why = if last_exit == LAUNCHD_EX_CONFIG {
            " launchd itself could not set the job up (EX_CONFIG), so baybo never ran — \
             typically it cannot open the binary or the log files."
        } else {
            ""
        };
        Err(InstallerError::Other(format!(
            "the gateway did not stay up: {}.{why}",
            ServiceStatus::Crashing { last_exit }
        )))
    }
}

/// The few top-level fields of `launchctl print` this installer acts on.
///
/// `launchctl print` has no stable machine format; only fields one tab deep
/// belong to the job itself — nested dicts (endpoints, sockets) repeat keys
/// like `state`, so a deeper line must never be read as the job's.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct JobState {
    pid: Option<u32>,
    runs: Option<u64>,
    /// `None` until the job has exited at least once.
    last_exit: Option<i32>,
}

impl JobState {
    fn parse(out: &str) -> Self {
        let mut state = Self::default();
        for line in out.lines() {
            let Some(field) = line.strip_prefix('\t') else {
                continue;
            };
            if field.starts_with('\t') {
                continue;
            }
            let Some((key, value)) = field.split_once(" = ") else {
                continue;
            };
            // `last exit code = 78: EX_CONFIG`, `(never exited)` before the first.
            let number = value.split(':').next().unwrap_or_default().trim();
            match key.trim() {
                "pid" => state.pid = number.parse().ok(),
                "runs" => state.runs = number.parse().ok(),
                "last exit code" => state.last_exit = number.parse().ok(),
                _ => {}
            }
        }
        state
    }

    fn status(&self) -> ServiceStatus {
        match (self.pid, self.last_exit) {
            (Some(_), _) => ServiceStatus::Running,
            (None, Some(code)) if code != 0 => ServiceStatus::Crashing { last_exit: code },
            _ => ServiceStatus::Enabled,
        }
    }
}

/// How to grant access in System Settings — the route for a decision that
/// has already gone against us, and the fallback for a dialog nobody saw.
fn settings_route(exec: &Path) -> String {
    format!(
        "System Settings → Privacy & Security → Files & Folders → baybo → switch the \
         volume on; or Full Disk Access → + → add {exec}. Either grant is tied to this \
         exact binary, so an upgrade may ask again. Or move the workspace to the internal \
         disk (and point BAYBO_CONFIG_PATH there).",
        exec = exec.display(),
    )
}

/// The probe is blocked on a TCC dialog that has not been answered.
fn pending_prompt_remedy(exec: &Path) -> String {
    format!(
        "macOS is most likely holding it on a permission dialog (\"baybo would like to \
         access files on a removable volume\" or similar). Click Allow and re-run. If no \
         dialog is on screen: {}",
        settings_route(exec)
    )
}

/// The probe was refused outright: a decision exists and it is "no".
fn denied_remedy(exec: &Path, probe_output: &str) -> String {
    let where_ = if probe_output.contains("/Volumes/") {
        "on another volume"
    } else {
        "in a protected folder (Documents, Desktop, iCloud, a network share)"
    };
    format!(
        "Part of your baybo workspace is {where_}, and macOS refuses background services \
         access to it. Interactive shells are allowed, which is why everything works from a \
         terminal. To fix: {}",
        settings_route(exec)
    )
}

struct AgentPlist<'a> {
    label: &'a str,
    exec: &'a Path,
    args: &'a [&'a str],
    stdout: &'a Path,
    stderr: &'a Path,
    /// Supplies the `PATH` / `BAYBO_CONFIG_PATH` the job runs with — the
    /// probe must see exactly the environment the gateway will.
    ctx: &'a InstallContext,
    /// `KeepAlive` for the gateway; a one-shot `RunAtLoad` for the probe.
    supervised: bool,
}

fn render_agent(p: &AgentPlist<'_>) -> String {
    let mut program = format!(
        "      <string>{}</string>\n",
        xml_escape(&p.exec.display().to_string())
    );
    for arg in p.args {
        program.push_str(&format!("      <string>{}</string>\n", xml_escape(arg)));
    }
    // Always emitted: a launchd agent's default `PATH` is narrower than
    // systemd's (no `/usr/local/bin`, so not even Homebrew), which leaves
    // every host tool baybo shells out to unresolvable. See
    // `resolve_service_path`.
    let mut env_block = String::from("    <key>EnvironmentVariables</key>\n    <dict>\n");
    env_block.push_str("      <key>PATH</key>\n");
    env_block.push_str(&format!(
        "      <string>{}</string>\n",
        xml_escape(&p.ctx.path_env)
    ));
    if let Some(cfg) = &p.ctx.config_path {
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
    let lifecycle = if p.supervised {
        "    <key>RunAtLoad</key>\n    <false/>\n    <key>KeepAlive</key>\n    <true/>\n    \
         <key>ThrottleInterval</key>\n    <integer>2</integer>\n"
    } else {
        "    <key>RunAtLoad</key>\n    <true/>\n    <key>KeepAlive</key>\n    <false/>\n"
    };
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
{program}    </array>
{lifecycle}    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
{env_block}  </dict>
</plist>
"#,
        label = p.label,
        stdout = xml_escape(&p.stdout.display().to_string()),
        stderr = xml_escape(&p.stderr.display().to_string()),
    )
}

fn create_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| InstallerError::Io {
        path: dir.display().to_string(),
        reason: e.to_string(),
    })
}

fn write_file(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body).map_err(|e| InstallerError::Io {
        path: path.display().to_string(),
        reason: e.to_string(),
    })
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
            log_dir: PathBuf::from("/Users/me/Library/Logs/baybo"),
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
        assert!(body.contains("<string>start</string>"));
        assert!(body.contains("BAYBO_CONFIG_PATH"));
        assert!(body.contains("/Users/me/.baybo/config/baybo.json"));
        assert!(body.contains("<key>KeepAlive</key>\n    <true/>"));
    }

    #[test]
    fn render_plist_without_config_path() {
        let inst = LaunchdInstaller::new();
        let mut c = ctx();
        c.config_path = None;
        let body = inst.render_unit(&c);
        assert!(!body.contains("BAYBO_CONFIG_PATH"));
    }

    #[test]
    fn render_plist_logs_to_the_context_log_dir() {
        let body = LaunchdInstaller::new().render_unit(&ctx());
        assert!(body.contains("/Users/me/Library/Logs/baybo/baybo-gateway.err.log"));
        assert!(body.contains("/Users/me/Library/Logs/baybo/baybo-gateway.out.log"));
    }

    #[test]
    fn service_logs_live_under_the_user_logs_folder_not_the_workspace() {
        let dir = LaunchdInstaller::new().log_dir(Path::new("/Volumes/data/baybo/logs"));
        assert!(dir.ends_with(SERVICE_LOG_SUBDIR), "{}", dir.display());
        assert!(!dir.starts_with("/Volumes"));
    }

    #[test]
    fn the_probe_is_one_shot_and_runs_preflight_with_the_gateway_env() {
        let c = ctx();
        let body = render_agent(&AgentPlist {
            label: PREFLIGHT_LABEL,
            exec: &c.exec_start,
            args: &["gateway", "preflight"],
            stdout: Path::new("/tmp/p/out"),
            stderr: Path::new("/tmp/p/err"),
            ctx: &c,
            supervised: false,
        });
        assert!(body.contains("<string>com.baybo.gateway.preflight</string>"));
        assert!(body.contains("<string>preflight</string>"));
        assert!(body.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(body.contains("<key>KeepAlive</key>\n    <false/>"));
        assert!(body.contains("/Users/me/.baybo/config/baybo.json"));
        assert!(body.contains("/opt/homebrew/bin:/usr/local/bin:/usr/bin"));
    }

    /// Trimmed from a real `launchctl print` of a gateway in a crash loop.
    const CRASH_LOOP: &str = "gui/501/com.baybo.gateway = {\n\
        \tactive count = 0\n\
        \tpath = /Users/me/Library/LaunchAgents/com.baybo.gateway.plist\n\
        \ttype = LaunchAgent\n\
        \tstate = spawn scheduled\n\
        \n\
        \tprogram = /Users/me/.local/bin/baybo\n\
        \truns = 68\n\
        \tlast exit code = 78: EX_CONFIG\n\
        \n\
        \tendpoints = {\n\
        \t\tstate = active\n\
        \t}\n\
        }\n";

    const RUNNING: &str = "gui/501/com.baybo.gateway = {\n\
        \tstate = running\n\
        \tpid = 9488\n\
        \truns = 3\n\
        \tlast exit code = 15: Terminated: 15\n\
        \t\tpid = 1\n\
        }\n";

    #[test]
    fn a_crash_loop_is_not_running() {
        let state = JobState::parse(CRASH_LOOP);
        assert_eq!(
            state,
            JobState {
                pid: None,
                runs: Some(68),
                last_exit: Some(LAUNCHD_EX_CONFIG)
            }
        );
        assert_eq!(state.status(), ServiceStatus::Crashing { last_exit: 78 });
    }

    #[test]
    fn a_pid_means_running_even_after_a_previous_exit() {
        let state = JobState::parse(RUNNING);
        assert_eq!(state.pid, Some(9488));
        assert_eq!(state.status(), ServiceStatus::Running);
    }

    #[test]
    fn nested_fields_never_read_as_the_jobs_own() {
        let state = JobState::parse("job = {\n\t\tpid = 1\n\t\truns = 9\n}\n");
        assert_eq!(state, JobState::default());
    }

    #[test]
    fn a_job_that_never_exited_is_enabled_not_crashing() {
        let state = JobState::parse("job = {\n\tlast exit code = (never exited)\n}\n");
        assert_eq!(state.last_exit, None);
        assert_eq!(state.status(), ServiceStatus::Enabled);
    }

    #[test]
    fn a_clean_exit_is_not_a_crash() {
        let state = JobState::parse("job = {\n\tlast exit code = 0\n}\n");
        assert_eq!(state.status(), ServiceStatus::Enabled);
    }

    #[test]
    fn the_remedy_names_the_binary_and_an_external_volume() {
        let r = denied_remedy(
            Path::new("/Users/me/.local/bin/baybo"),
            "open /Volumes/data/baybo/config/baybo.json: Operation not permitted",
        );
        assert!(r.contains("/Users/me/.local/bin/baybo"));
        assert!(r.contains("another volume"));
        assert!(r.contains("Full Disk Access"));
    }

    #[test]
    fn a_hung_probe_points_at_the_dialog_first() {
        let r = pending_prompt_remedy(Path::new("/Users/me/.local/bin/baybo"));
        assert!(r.contains("Click Allow"));
        assert!(r.contains("Files & Folders"));
    }
}
