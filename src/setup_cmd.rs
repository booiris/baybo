//! Binary entry for `aura setup`. Runs ahead of `boot::load_config`
//! because the wizard's job is to create the workspace + key +
//! `aura.json` that the rest of the binary expects.

use aura_setup::{SetupOutcome, TtyPrompter};
use aura_workspace::paths::default_workspace_root;

use crate::tracing_init::{TracingMode, init_tracing};

pub async fn run() -> anyhow::Result<()> {
    let _tracing_guards = init_tracing(TracingMode::Stdout);

    let workspace_root = default_workspace_root();
    eprintln!("Setting up Aura workspace at {}", workspace_root.display());

    let mut ctx = aura_setup::bootstrap_workspace_if_needed(workspace_root)
        .await
        .map_err(|e| anyhow::anyhow!("workspace bootstrap failed: {e}"))?;

    let mut prompter =
        TtyPrompter::new().map_err(|e| anyhow::anyhow!("setup is interactive: {e}"))?;

    let outcome = aura_setup::run(&mut prompter, &mut ctx)
        .await
        .map_err(|e| anyhow::anyhow!("setup wizard failed: {e}"))?;

    print_summary(&outcome);

    // Setup never starts the gateway itself — it points the operator at
    // the next command. `aura gateway start` then prints the dashboard
    // URL and admin token.
    aura_setup::print_exit_hint(&ctx.config_path);
    Ok(())
}

fn print_summary(outcome: &SetupOutcome) {
    eprintln!();
    eprintln!("Setup complete:");
    eprintln!("  mode:    {:?}", outcome.mode);
    eprintln!(
        "  llm:     {}",
        outcome.llm_added.as_deref().unwrap_or("(skipped)")
    );
    eprintln!(
        "  channel: {}",
        outcome.channel_added.as_deref().unwrap_or("(skipped)")
    );
    eprintln!(
        "  browser: {}",
        if outcome.browser_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    eprintln!();
}
