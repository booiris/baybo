//! Wizard orchestrator — mode picker, step sequence, β2 commit, and
//! the post-setup gateway-launch / exit picker.

use std::path::Path;
use std::time::{Duration, Instant};

use aura_gateway::{LISTENING_BANNER_PREFIX, SidecarRuntime};

use crate::bootstrap::SetupContext;
use crate::error::{Result, SetupError};
use crate::flow::{
    BrowserStepOutcome, ChannelStepOutcome, ExternalAgentsStepOutcome, LlmStepOutcome,
    configure_browser_step, configure_channel_step, configure_external_agents_step,
    configure_llm_step,
};
use crate::prompt::Prompter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupMode {
    Quick,
    Full,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetupOutcome {
    pub mode: SetupMode,
    pub llm_added: Option<String>,
    pub channel_added: Option<String>,
    pub browser_enabled: bool,
    pub external_agents: ExternalAgentsStepOutcome,
}

pub async fn run<P: Prompter>(prompter: &mut P, ctx: &mut SetupContext) -> Result<SetupOutcome> {
    let modes = ["Quick setup", "Full setup"];
    match prompter.select("Setup mode:", &modes)? {
        0 => run_quick(prompter, ctx).await,
        _ => run_full(prompter, ctx).await,
    }
}

pub async fn run_quick<P: Prompter>(
    prompter: &mut P,
    ctx: &mut SetupContext,
) -> Result<SetupOutcome> {
    let llm_added = run_llm_step(prompter, ctx).await?;
    print_step_separator();
    let channel_added = run_channel_step(prompter, ctx).await?;
    print_step_separator();

    // Quick mode auto-enables the browser tool with docker mode on. The
    // sidecar logs and falls back to host-headless when docker isn't
    // reachable (or on macOS, where docker mode is upstream-disabled),
    // so flipping these on is safe regardless of host.
    ctx.config.browser.enable = true;
    ctx.config.browser.docker.enable = true;
    eprintln!("Browser tool: enabled (docker mode; falls back to host-headless if unavailable)");
    print_step_separator();

    let external_agents = configure_external_agents_step(prompter, &mut ctx.config).await?;
    print_step_separator();

    commit_config(ctx).await?;

    Ok(SetupOutcome {
        mode: SetupMode::Quick,
        llm_added,
        channel_added,
        browser_enabled: ctx.config.browser.enable,
        external_agents,
    })
}

pub async fn run_full<P: Prompter>(
    prompter: &mut P,
    ctx: &mut SetupContext,
) -> Result<SetupOutcome> {
    let llm_added = run_llm_step(prompter, ctx).await?;
    print_step_separator();
    let channel_added = run_channel_step(prompter, ctx).await?;
    print_step_separator();

    let browser = configure_browser_step(prompter, &mut ctx.config).await?;
    let browser_enabled = matches!(browser, BrowserStepOutcome::Enabled { .. });
    print_step_separator();

    let external_agents = configure_external_agents_step(prompter, &mut ctx.config).await?;
    print_step_separator();

    commit_config(ctx).await?;

    Ok(SetupOutcome {
        mode: SetupMode::Full,
        llm_added,
        channel_added,
        browser_enabled,
        external_agents,
    })
}

/// Visual divider printed to stderr after each step finishes so the
/// operator can tell where one module's output ends and the next begins.
fn print_step_separator() {
    eprintln!();
    eprintln!("──────────────────────────────────────────────");
    eprintln!();
}

async fn run_llm_step<P: Prompter>(
    prompter: &mut P,
    ctx: &mut SetupContext,
) -> Result<Option<String>> {
    let allow_skip = !ctx.config.llm.is_empty();
    let outcome = configure_llm_step(prompter, &ctx.vault, &mut ctx.config, allow_skip).await?;
    Ok(match outcome {
        LlmStepOutcome::Added(entry) => Some(entry.name),
        LlmStepOutcome::Skipped => None,
    })
}

async fn run_channel_step<P: Prompter>(
    prompter: &mut P,
    ctx: &mut SetupContext,
) -> Result<Option<String>> {
    let runtime = SidecarRuntime::install().map_err(|e| {
        SetupError::Channel(format!(
            "sidecar runtime unavailable ({e}); rebuild with an embedded bundle"
        ))
    })?;
    let outcome = configure_channel_step(
        prompter,
        &runtime,
        &ctx.stores.channel_bot,
        &ctx.vault,
        /*allow_skip=*/ true,
    )
    .await?;
    Ok(match outcome {
        ChannelStepOutcome::Added(reg) => {
            Some(format!("{}/{}", reg.channel_type.as_str(), reg.bot_id))
        }
        ChannelStepOutcome::Skipped => None,
    })
}

async fn commit_config(ctx: &mut SetupContext) -> Result<()> {
    ctx.config
        .validate()
        .map_err(|e| SetupError::Config(format!("final validation failed: {e}")))?;
    ctx.config
        .write_to_file(&ctx.config_path)
        .await
        .map_err(|e| SetupError::Config(format!("write {}: {e}", ctx.config_path.display())))?;
    tracing::info!(path = %ctx.config_path.display(), "wrote final aura.json");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndAction {
    LaunchGatewayAndOpenBrowser,
    Exit,
}

pub fn pick_end_action<P: Prompter>(prompter: &mut P) -> Result<EndAction> {
    let options = [
        "Start gateway and open dashboard (default)",
        "Exit (run `aura gateway start` later)",
    ];
    let idx = prompter.select("What now?", &options)?;
    Ok(if idx == 0 {
        EndAction::LaunchGatewayAndOpenBrowser
    } else {
        EndAction::Exit
    })
}

/// Spawn `aura gateway start` (resolved via `current_exe`), wait for
/// its bind banner, open the dashboard URL, then wait on the child so
/// SIGINT propagates.
pub async fn launch_gateway_and_open_browser() -> Result<()> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let exe = std::env::current_exe()
        .map_err(|e| SetupError::Gateway(format!("resolve current exe: {e}")))?;

    let mut cmd = Command::new(&exe);
    cmd.arg("gateway")
        .arg("start")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        SetupError::Gateway(format!("spawn `{} gateway start`: {e}", exe.display()))
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| SetupError::Gateway("gateway child stdout missing".into()))?;
    let mut reader = BufReader::new(stdout).lines();

    // 30s cap so a broken-config gateway-start doesn't hang setup forever.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut url: Option<String> = None;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = match tokio::time::timeout(remaining, reader.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return Err(SetupError::Gateway(format!("read gateway stdout: {e}"))),
            Err(_) => break,
        };
        println!("{line}");
        if let Some(rest) = line.strip_prefix(LISTENING_BANNER_PREFIX) {
            url = Some(rest.trim().to_string());
            break;
        }
    }

    let dashboard_url = url.ok_or_else(|| {
        SetupError::Gateway(
            "gateway didn't print its 'listening on' banner within 30s; \
             check stderr above for the underlying error"
                .into(),
        )
    })?;

    // Browser-open failure is non-fatal: operator can visit the URL by hand.
    if let Err(e) = open::that(&dashboard_url) {
        tracing::warn!(
            error = %e,
            url = %dashboard_url,
            "failed to open dashboard URL in browser; visit it manually"
        );
    } else {
        tracing::info!(url = %dashboard_url, "opened dashboard in browser");
    }

    let pump = tokio::spawn(async move {
        while let Ok(Some(line)) = reader.next_line().await {
            println!("{line}");
        }
    });

    let status = child
        .wait()
        .await
        .map_err(|e| SetupError::Gateway(format!("wait gateway child: {e}")))?;
    let _ = pump.await;

    if !status.success() {
        return Err(SetupError::Gateway(format!("gateway exited with {status}")));
    }
    Ok(())
}

pub fn print_exit_hint(config_path: &Path) {
    println!();
    println!("Setup complete. Wrote {}.", config_path.display());
    println!();
    println!("Next steps:");
    println!("  aura gateway start    # start the daemon (dashboard at the printed URL)");
    println!("  aura tui              # interactive terminal UI (needs gateway running)");
    println!();
}
