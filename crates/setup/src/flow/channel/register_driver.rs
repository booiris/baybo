//! Channel sidecar registration driver.
//!
//! Spawns the bundled `bun` sidecar in registration mode, drives the
//! stdin/stdout JSON exchange (`RegisterIn` / `RegisterOut`), and
//! returns the credentials it emits. Lifted from the previous
//! `crates/cli/src/commands/channel/register.rs` — only the error
//! type changed (`CliError` → `SetupError`).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use aura_channels::register_wire::{MAX_FRAME_BYTES, PromptKind, RegisterIn, RegisterOut};
use aura_channels::registration::{Prompter as ChannelPrompter, RegistrationResult};
use aura_gateway::{BUN_BINARY_ENV, SIDECAR_ENV_ALLOWLIST, SidecarRuntime};
use aura_model::ChannelType;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};

use crate::error::{Result, SetupError};

const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Spawn the channel's bundled sidecar in registration mode, drive the
/// stdin/stdout JSON exchange, and return the credentials it emits.
///
/// `timeout` is the overall deadline; on expiry the subprocess is
/// SIGKILLed via `kill_on_drop(true)`.
pub async fn run_registration(
    runtime: &SidecarRuntime,
    channel_type: &ChannelType,
    prompter: &mut dyn ChannelPrompter,
    timeout: Duration,
) -> Result<RegistrationResult> {
    let bundle = runtime.bundle_for(channel_type.as_str()).ok_or_else(|| {
        SetupError::Channel(format!(
            "no embedded sidecar bundle for '{}' in this build",
            channel_type.as_str()
        ))
    })?;

    let mut cmd = Command::new(bun_binary());
    cmd.arg(bundle);
    scrubbed_env(&mut cmd);
    drive(cmd, prompter, timeout).await
}

fn bun_binary() -> PathBuf {
    std::env::var_os(BUN_BINARY_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("bun"))
}

async fn drive(
    mut cmd: Command,
    prompter: &mut dyn ChannelPrompter,
    timeout: Duration,
) -> Result<RegistrationResult> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| SetupError::Channel(format!("spawn registration subprocess: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| SetupError::Channel("child stdin was piped but missing".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SetupError::Channel("child stdout was piped but missing".into()))?;

    let result = tokio::time::timeout(timeout, pump(stdout, stdin, prompter))
        .await
        .map_err(|_| {
            SetupError::Channel(format!(
                "registration timed out after {}s",
                timeout.as_secs()
            ))
        })??;

    // Once the sidecar has emitted its `Result` frame we have the
    // credentials we need. Subsequent shutdown problems get logged
    // for diagnostics but do not invalidate the captured result.
    match tokio::time::timeout(EXIT_GRACE, child.wait()).await {
        Ok(Ok(status)) if !status.success() => {
            tracing::warn!(
                ?status,
                "sidecar exited non-zero after emitting registration result; \
                 proceeding with the credentials it emitted"
            );
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "wait for sidecar exit after registration result");
        }
        Err(_) => {
            tracing::warn!(
                grace_secs = EXIT_GRACE.as_secs(),
                "sidecar did not exit within grace after registration result; killing"
            );
        }
    }

    Ok(result)
}

async fn pump(
    stdout: ChildStdout,
    mut stdin: ChildStdin,
    prompter: &mut dyn ChannelPrompter,
) -> Result<RegistrationResult> {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();

    loop {
        buf.clear();
        let eof = read_line_capped(&mut reader, &mut buf, MAX_FRAME_BYTES).await?;
        if eof && buf.is_empty() {
            return Err(SetupError::Channel(
                "sidecar closed stdout without emitting a result".into(),
            ));
        }
        let msg: RegisterOut = serde_json::from_slice(&buf)
            .map_err(|e| SetupError::Channel(format!("parse sidecar frame: {e}")))?;

        match msg {
            RegisterOut::Prompt {
                id,
                label,
                kind,
                required,
            } => {
                let value = match kind {
                    PromptKind::Input => prompter.input(&label, required),
                    PromptKind::Password => prompter.password(&label, required),
                }
                .map_err(|e| SetupError::Channel(format!("prompt failed: {e}")))?;
                let reply = RegisterIn::PromptReply { id, value };
                let mut bytes = serde_json::to_vec(&reply)
                    .map_err(|e| SetupError::Channel(format!("serialize prompt reply: {e}")))?;
                bytes.push(b'\n');
                stdin
                    .write_all(&bytes)
                    .await
                    .map_err(|e| SetupError::Channel(format!("write prompt reply: {e}")))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| SetupError::Channel(format!("flush prompt reply: {e}")))?;
            }
            RegisterOut::Result {
                bot_id,
                token,
                metadata,
                secrets,
            } => {
                return Ok(RegistrationResult {
                    bot_id,
                    token,
                    metadata,
                    secrets,
                });
            }
            RegisterOut::Error { message } => {
                return Err(SetupError::Channel(format!(
                    "sidecar registration error: {message}"
                )));
            }
        }
    }
}

async fn read_line_capped<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> Result<bool> {
    let mut byte = [0u8; 1];
    loop {
        let n = reader
            .read(&mut byte)
            .await
            .map_err(|e| SetupError::Channel(format!("read sidecar stdout: {e}")))?;
        if n == 0 {
            return Ok(true);
        }
        if byte[0] == b'\n' {
            return Ok(false);
        }
        if buf.len() >= max {
            return Err(SetupError::Channel(format!(
                "sidecar frame exceeds {max} bytes"
            )));
        }
        buf.push(byte[0]);
    }
}

fn scrubbed_env(cmd: &mut Command) {
    cmd.env_clear();
    for key in SIDECAR_ENV_ALLOWLIST {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("AURA_CHANNEL_MODE", "register");
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct FakePrompter {
        answers: VecDeque<String>,
    }

    impl FakePrompter {
        fn new<I, S>(answers: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: Into<String>,
        {
            Self {
                answers: answers.into_iter().map(Into::into).collect(),
            }
        }
    }

    impl ChannelPrompter for FakePrompter {
        fn input(&mut self, label: &str, _required: bool) -> anyhow::Result<String> {
            self.answers
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("FakePrompter out of answers for input({label})"))
        }
        fn password(&mut self, label: &str, _required: bool) -> anyhow::Result<String> {
            self.answers
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("FakePrompter out of answers for password({label})"))
        }
    }

    fn bash(script: &str) -> Command {
        let mut cmd = Command::new("/bin/bash");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[tokio::test]
    async fn happy_path_single_prompt() {
        let cmd = bash(
            r#"printf '{"type":"prompt","id":"p1","label":"lbl","kind":"password","required":true}\n'
               read REPLY
               printf '%s\n' "$REPLY" >&2
               printf '{"type":"result","bot_id":"123","token":"tok"}\n'"#,
        );
        let mut prompter = FakePrompter::new(["secret"]);
        let out = drive(cmd, &mut prompter, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(out.bot_id, "123");
        assert_eq!(out.token, "tok");
    }

    #[tokio::test]
    async fn sidecar_error_surfaces() {
        let cmd = bash(r#"printf '{"type":"error","message":"user cancelled"}\n'"#);
        let mut prompter = FakePrompter::new(Vec::<&str>::new());
        let err = drive(cmd, &mut prompter, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("user cancelled"), "got: {err}");
    }

    #[tokio::test]
    async fn closes_without_result_is_error() {
        let cmd = bash(r#"exit 0"#);
        let mut prompter = FakePrompter::new(Vec::<&str>::new());
        let err = drive(cmd, &mut prompter, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("without emitting"), "got: {err}");
    }

    #[tokio::test]
    async fn oversize_frame_rejected() {
        let cmd = bash(&format!(
            r#"head -c {} /dev/zero | tr '\0' 'a'; printf '\n'"#,
            MAX_FRAME_BYTES + 1
        ));
        let mut prompter = FakePrompter::new(Vec::<&str>::new());
        let err = drive(cmd, &mut prompter, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds"), "got: {err}");
    }

    #[tokio::test]
    async fn timeout_kills_subprocess() {
        let cmd = bash(r#"sleep 30"#);
        let mut prompter = FakePrompter::new(Vec::<&str>::new());
        let err = drive(cmd, &mut prompter, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    #[tokio::test]
    async fn non_zero_exit_after_result_does_not_fail() {
        let cmd = bash(r#"printf '{"type":"result","bot_id":"b","token":"t"}\n'; exit 7"#);
        let mut prompter = FakePrompter::new(Vec::<&str>::new());
        let out = drive(cmd, &mut prompter, Duration::from_secs(5))
            .await
            .expect("post-result exit status must not invalidate the captured result");
        assert_eq!(out.bot_id, "b");
        assert_eq!(out.token, "t");
    }
}
