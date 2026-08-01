//! Wizard orchestrator — mode picker, step sequence, β2 commit, and
//! the post-setup exit hint.

use std::path::Path;

use baybo_gateway::SidecarRuntime;

use crate::bootstrap::SetupContext;
use crate::error::{Result, SetupError};
use crate::flow::{
    BrowserStepOutcome, ChannelStepOutcome, ExternalAgentsStepOutcome, LlmStepOutcome,
    configure_browser_step, configure_channel_step, configure_external_agents_step,
    configure_llm_step,
};
use crate::prompt::Prompter;
use crate::tty::PROMPT_DIVIDER;

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

    // Quick mode auto-enables the browser tool with docker mode on. The
    // sidecar logs and falls back to host-headless when docker isn't
    // reachable (or on macOS, where docker mode is upstream-disabled),
    // so flipping these on is safe regardless of host.
    ctx.config.browser.enable = true;
    ctx.config.browser.docker.enable = true;
    eprintln!("Browser tool: enabled (docker mode; falls back to host-headless if unavailable)");
    print_step_separator();

    commit_config(ctx).await?;

    Ok(SetupOutcome {
        mode: SetupMode::Quick,
        llm_added,
        channel_added: None,
        browser_enabled: ctx.config.browser.enable,
        external_agents: ExternalAgentsStepOutcome::default(),
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
    eprintln!("{PROMPT_DIVIDER}");
    eprintln!();
}

async fn run_llm_step<P: Prompter>(
    prompter: &mut P,
    ctx: &mut SetupContext,
) -> Result<Option<String>> {
    let allow_skip = !ctx.config.llm.is_empty();
    let outcome = configure_llm_step(prompter, &ctx.vault, &mut ctx.config, allow_skip).await?;
    Ok(match outcome {
        LlmStepOutcome::Added(entry) => Some(entry.name.to_string()),
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
    tracing::info!(path = %ctx.config_path.display(), "wrote final baybo.json");
    Ok(())
}

pub fn print_exit_hint(config_path: &Path) {
    println!();
    println!("Setup complete. Wrote {}.", config_path.display());
    println!();
    println!("Next steps:");
    println!("  baybo gateway start    # start the daemon (dashboard at the printed URL)");
    println!("  baybo tui              # interactive terminal UI (needs gateway running)");
    println!();
}

#[cfg(test)]
mod tests {
    use baybo_config::LlmEntry;
    use tempfile::tempdir;

    use super::*;
    use crate::bootstrap_workspace_if_needed;
    use crate::test_support::MockPrompter;

    #[tokio::test]
    async fn quick_setup_skips_channel_and_external_agent_steps() {
        let temp = tempdir().unwrap();
        let mut ctx = bootstrap_workspace_if_needed(temp.path().to_path_buf())
            .await
            .unwrap();
        ctx.config.llm.push(LlmEntry {
            name: "local".into(),
            provider: "llamafile".into(),
            model: "local-model".into(),
            model_list: Vec::new(),
            lite_model: Some("local-model".into()),
            api_key_env: None,
            base_url: Some("http://localhost:8080".into()),
            reasoning_effort: None,
        });
        ctx.config.default_llm = "local".into();
        ctx.config.external_agents.claude.enabled = true;
        ctx.config.external_agents.claude.binary_path = Some("/usr/bin/claude".into());
        let external_agents_before = ctx.config.external_agents.clone();
        let channels_before = ctx.config.channels.clone();
        let mut prompter = MockPrompter::new().push_select(1);

        let outcome = run_quick(&mut prompter, &mut ctx).await.unwrap();

        assert_eq!(outcome.mode, SetupMode::Quick);
        assert_eq!(outcome.channel_added, None);
        assert_eq!(
            outcome.external_agents,
            ExternalAgentsStepOutcome::default()
        );
        assert_eq!(ctx.config.channels, channels_before);
        assert_eq!(ctx.config.external_agents, external_agents_before);
        assert!(ctx.config.browser.enable);
        assert!(ctx.config.browser.docker.enable);
        assert!(prompter.selects.is_empty());
        assert!(prompter.multi_selects.is_empty());
    }
}
