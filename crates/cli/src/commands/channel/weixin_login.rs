//! Spawn the embedded weixin sidecar bundle in `--login` mode, stream
//! the QR rendering to the operator's terminal, and capture the final
//! `AURA_WEIXIN_LOGIN_RESULT:{…}` marker from the child's stdout.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use aura_channels::registration::{WeixinAuthBlob, WeixinLoginRunner, parse_login_result_line};
use aura_gateway::SidecarRuntime;

pub struct SidecarLoginRunner {
    runtime: Arc<SidecarRuntime>,
}

impl SidecarLoginRunner {
    /// Materialise the embedded bun + weixin bundle if they aren't
    /// already on disk, then hand back a runner the registration flow
    /// can invoke. Returns `None` when the current build doesn't embed
    /// a weixin bundle (degraded `build.rs`); callers omit the weixin
    /// entry from the registration catalog in that case.
    pub fn try_new() -> anyhow::Result<Option<Self>> {
        let runtime = SidecarRuntime::install().map_err(|e| anyhow::anyhow!(e))?;
        if runtime.bundle_for("weixin").is_none() {
            return Ok(None);
        }
        Ok(Some(Self {
            runtime: Arc::new(runtime),
        }))
    }
}

impl WeixinLoginRunner for SidecarLoginRunner {
    fn run_login(&self) -> anyhow::Result<WeixinAuthBlob> {
        let bun = self.runtime.bun_path();
        let bundle = self
            .runtime
            .bundle_for("weixin")
            .ok_or_else(|| anyhow::anyhow!("weixin sidecar bundle unavailable in this build"))?;

        let mut cmd = Command::new(bun);
        cmd.arg(bundle)
            .env("AURA_WEIXIN_MODE", "login")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn weixin login process ({}): {e}", bun.display()))?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let mut reader = BufReader::new(stdout);
        let mut stderr_sink = std::io::stderr();

        let mut blob: Option<WeixinAuthBlob> = None;
        let mut line = String::new();
        loop {
            line.clear();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            match parse_login_result_line(&line) {
                Ok(Some(b)) => {
                    blob = Some(b);
                    break;
                }
                Ok(None) => {
                    // Forward non-marker stdout to the operator's
                    // terminal so qrcode-terminal block characters
                    // render live; stderr is already inherited.
                    let _ = stderr_sink.write_all(line.as_bytes());
                    let _ = stderr_sink.flush();
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
            }
        }

        let status = child.wait()?;

        let Some(blob) = blob else {
            if status.success() {
                anyhow::bail!("weixin login process exited without emitting a login result");
            }
            anyhow::bail!("weixin login process failed (exit status: {})", status);
        };
        if !status.success() {
            anyhow::bail!(
                "weixin login process exited with status {} after emitting a result",
                status,
            );
        }
        Ok(blob)
    }
}
