mod boot;
mod gateway_client;
mod gateway_cmd;
mod prompt_cmd;
mod reload;
mod runtime;
mod sandbox_boot;
mod setup_cmd;
mod tracing_init;
mod tui_cmd;
mod tui_log;

use baybo_cli::cli::ShellKind;
use baybo_cli::{Cli, Commands, ContextBuilder, Invocation, OutputFormat, dispatch};
use clap::CommandFactory;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

use crate::tracing_init::{TracingMode, init_tracing};

/// Resolve the effective baybo.json path, if any, for display in
/// diagnostics. Thin wrapper so existing callers keep working; the real
/// implementation lives in `boot` so `gateway_cmd` can reuse it without
/// depending on `main.rs`.
fn resolve_config_path() -> Option<PathBuf> {
    boot::resolve_config_path()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `parse_args` checks `BAYBO_HELP_AGENT` and swaps in an unhidden
    // `Command` before clap parses argv; that's how the env-var
    // surfaces extended `--help` output without needing a flag.
    let cli = baybo_cli::cli::parse_args();

    // Promote `--config <path>` into `BAYBO_CONFIG_PATH` before any
    // reader runs, so every downstream caller can stay env-only with a
    // single source of truth.
    //
    // SAFETY: `setenv` is process-global; Rust 2024 marks it unsafe
    // because a concurrent `getenv` on another thread can observe a
    // torn read on some libc implementations. We write exactly once,
    // exactly here, before `boot::load_config` (the only reader in this
    // binary) runs, and no other code in the process mutates or reads
    // `BAYBO_CONFIG_PATH` concurrently — so the race window is empty in
    // practice.
    if let Some(path) = cli.global.config.as_deref() {
        unsafe {
            std::env::set_var(baybo_workspace::paths::ENV_CONFIG_PATH, path);
        }
    }

    // `completion` is the only subcommand that must work without loading
    // config or initialising tracing — it is pure stdout output.
    if let Some(Commands::Completion { shell }) = cli.command {
        print_completion(shell)?;
        return Ok(());
    }

    // Gateway subcommands have their own entrypoint: they run without
    // the chat-loop boot path (install/status) or with a lightweight
    // vault-only boot (enable/token). `start` becomes a long-lived
    // server. Route them here before the generic argv/chat branch.
    if let Some(Commands::Gateway { cmd }) = cli.command {
        return gateway_cmd::run(cmd).await;
    }

    // `setup` is the first-run wizard. It bootstraps the workspace +
    // master key + default baybo.json before any of the normal
    // `boot::load_config` machinery can run, so it gets its own
    // entry point here, ahead of the argv/chat dispatch.
    if let Some(Commands::Setup) = cli.command.as_ref() {
        return setup_cmd::run().await;
    }

    // Bare `baybo` (no subcommand) prints help and exits. The interactive
    // chat loop is reached via the explicit `baybo tui` subcommand, and
    // one-shot answering via `baybo prompt`, so the default invocation
    // doesn't surprise users with a full-screen app.
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }

    let cli_format = pick_format(&cli);

    let config = boot::load_config().await?;
    let config = Arc::new(config);
    let workspace_paths =
        baybo_workspace::WorkspacePaths::new(PathBuf::from(&config.workspace.path));
    baybo_workspace::ensure_layout(&workspace_paths).await?;

    // One-shot `baybo prompt`: stream a single answer and exit. Sits
    // beside the `tui` early-return because both drive the agent runtime
    // (via a live gateway or in-process) rather than going through the
    // generic argv dispatch.
    if let Some(Commands::Prompt {
        prompt,
        session,
        dangerously_allow_all,
        timeout,
    }) = cli.command.as_ref()
    {
        let opts = prompt_cmd::Options {
            prompt: resolve_prompt(prompt.clone().unwrap_or_default())?,
            session: session.clone(),
            allow_all: *dangerously_allow_all,
            json: cli.global.json,
            timeout: (*timeout != 0).then(|| std::time::Duration::from_secs(*timeout)),
        };
        return prompt_cmd::run(config, opts).await;
    }

    // TUI: ratatui owns stdout, so tracing goes to a rolling file under
    // `<workspace>/logs/` (redacted through the shared `LeakDetector`)
    // and warn/error events mirror into the chat scrollback via
    // `TuiLogLayer`. Gets its own early return so the rest of `main`
    // doesn't need to branch on chat_mode.
    if let Some(Commands::Tui {
        session,
        #[cfg(debug_assertions)]
        dev_auto_gateway,
    }) = cli.command.as_ref()
    {
        let opts = tui_cmd::Options {
            session: session.clone(),
            #[cfg(debug_assertions)]
            dev_auto_gateway: *dev_auto_gateway,
        };
        return tui_cmd::run(config, opts).await;
    }

    // ---------------- argv dispatch (one-shot command + exit) ----------------
    //
    // Everything reaching this point is a one-shot argv command (Tui,
    // Completion, Gateway, None are all handled above). Argv mode builds
    // only the lightweight inspection set (skills, tools, channels,
    // workspace, optional LLM). It opens storage for BlobStore-backed
    // tool metadata, but avoids the full manager graph and turn recovery.
    init_tracing(TracingMode::Stdout);

    let cmd = cli.command.expect("non-command branches handled above");

    let skill_registry = if needs_skills(&cmd) {
        let reg = Arc::new(baybo_skills::SkillRegistry::new());
        let builtins = reg.register_builtins();
        if builtins > 0 {
            info!(count = builtins, "registered built-in skills");
        }
        // The built-in's own directory, eagerly: every other agent is loaded
        // lazily at actor build because the set of agents is DB state, but
        // this one is the default scope and is read by listings that run
        // before any actor exists.
        let builtin = baybo_model::AgentProfileId::builtin();
        let loaded = reg.ensure_agent_skills(&builtin, &workspace_paths);
        if loaded > 0 {
            info!(
                count = loaded,
                path = %builtin.skills_dir(&workspace_paths).display(),
                "loaded skills from the built-in persona"
            );
        }
        reg
    } else {
        Arc::new(baybo_skills::SkillRegistry::new())
    };
    let stores = baybo_storage::Store::open(boot::storage_db_path(&config.workspace)).await?;
    // Argv-mode commands (`llm probe`, `doctor`, `status`, `channel add`,
    // …) don't drive WebFetch through an agent loop; the per-call
    // `ToolContext::llm` is left `None` in those paths, so WebFetch
    // silently falls back to raw markdown.
    let tool_proxy = boot::proxy_settings(&config)
        .as_ref()
        .map(|p| p.to_proxy())
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid proxy.url: {e}"))?;
    let tool_registry = Arc::new(baybo_tools::ToolRegistry::with_defaults(
        baybo_tools::builtin::DefaultToolsConfig {
            blob_store: stores.blob.clone(),
            process_manager: baybo_process::ProcessManager::transient(),
            workspace_paths: workspace_paths.clone(),
            proxy: tool_proxy,
            // argv one-shots (llm/doctor/status) barely touch Bash; there is
            // no config reloader on this path, so use the default Bash
            // permission policy.
            permission: Arc::new(baybo_tools::builtin::LivePermissionMode::new(
                baybo_tools::builtin::BashPermissionMode::default(),
            )),
            builtin_memory: config.memory.builtin.enabled,
        },
    ));
    let workspace = Arc::new(workspace_paths.clone());
    let channels_registry = Arc::new(baybo_channels::ChannelRegistry::new());
    // Only `llm`, `doctor`, and `status` touch `ctx.llm` in the argv
    // path. Building the client unconditionally meant every run of
    // `baybo channel add` / `baybo config get` / etc. emitted a warn-level
    // "LLM client unavailable" message when no API key was configured,
    // which users reasonably interpreted as a hard error.
    let llm_client = if needs_llm(&cmd) {
        // `llm` / `doctor` / `status` never send multimodal content,
        // so it's fine to skip the BlobStore wiring here — opening
        // sqlite for a status probe would be wasteful.
        // No vault here either: argv-mode `llm` / `doctor` / `status` are
        // probes that don't need OAuth tokens; the openai-subscription
        // provider's create() returns a clear error if it's selected
        // without a vault.
        let provider_registry = baybo_llm::LlmProviderRegistry::with_default_providers();
        match boot::build_llm_client(
            &config,
            &provider_registry,
            None,
            None,
            baybo_llm::CostHooks::passthrough(),
        )
        .await
        {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "LLM client unavailable for this command");
                None
            }
        }
    } else {
        None
    };

    let mut builder = ContextBuilder::new(Arc::clone(&config))
        .config_path(resolve_config_path())
        .skills(Arc::clone(&skill_registry))
        .tools(Arc::clone(&tool_registry))
        .channels(Arc::clone(&channels_registry))
        .workspace(Arc::clone(&workspace));
    if let Some(ref client) = llm_client {
        builder = builder.llm(Arc::clone(client));
    }
    if needs_query_graph(&cmd) {
        // Monitoring commands (`status --live`, `cost show`, `log session`,
        // `session`, `turn`, `cron`) need the heavier domain graph that
        // argv-mode skips by default. Build the smallest set that lets
        // those handlers (and the auto-derived `QueryApi`) work, without
        // dragging in actors, supervisors, or LLM-side dependencies.
        //
        // `CronScheduler` needs a trigger channel and a `Shutdown`, but
        // we never call `.run()` here — only its read APIs. The dropped
        // receiver is fine: nothing in argv would push a trigger anyway,
        // and `ShutdownSignal::new()` returns an un-fired signal.
        builder = builder
            .session(Arc::new(baybo_agent::SessionManager::new(
                stores.session.clone(),
                stores.session_folder.clone(),
            )))
            .turn(Arc::new(baybo_turn::TurnLifecycle::new(
                stores.turn.clone(),
            )))
            .trace(stores.trace.clone())
            .cost_store(stores.cost.clone());
        let (cron_tx, _cron_rx) = tokio::sync::mpsc::channel(1);
        let shutdown: Arc<dyn baybo_cron::Shutdown> = Arc::new(baybo_agent::ShutdownSignal::new());
        builder = builder.cron(Arc::new(baybo_agent::CronScheduler::new(
            stores.cron.clone(),
            cron_tx,
            shutdown,
        )));
    }
    if let Ok((vault, stores)) = runtime::build_bot_registry_deps(&config).await {
        let device_service = std::sync::Arc::new(baybo_pairing::DevicePairingService::new(
            stores.device.clone(),
        ));
        builder = builder
            .secret_vault(vault)
            .channel_bot_store(stores.channel_bot)
            .channel_pairing_store(stores.channel_pairing)
            .device_pairing_service(device_service);
    }
    let ctx = builder
        .build()
        .with_format(cli_format)
        .with_invocation(Invocation::Argv);

    match dispatch::run(&ctx, cmd).await {
        Ok(out) => {
            let rendered = out.render(cli_format);
            if !rendered.is_empty() {
                println!("{rendered}");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Subcommands whose handlers actually read `ctx.llm`. Everything
/// else (channel/config/memory/session/skills/…) can boot without an
/// LLM provider configured — they must not trip the bootstrap warning
/// just by running.
///
/// `baybo llm` subcommands intentionally aren't here: their handlers
/// construct their own provider client with the vault wired in (see
/// `cli/src/commands/llm.rs::probe`). Pre-building here would also
/// fail for the openai-subscription default-llm because argv mode
/// passes `None` for the vault.
fn needs_llm(cmd: &Commands) -> bool {
    matches!(cmd, Commands::Doctor | Commands::Status { .. })
}

/// Subcommands that read `ctx.skills`. Anything else gets an empty
/// `SkillRegistry`, skipping the per-invocation built-in registration
/// and on-disk SKILL.md scan that would otherwise fire for every
/// `baybo config get`, `baybo cost show`, etc.
fn needs_skills(cmd: &Commands) -> bool {
    matches!(cmd, Commands::Skills { .. } | Commands::Status { .. })
}

/// Subcommands that read `ctx.session` / `ctx.turn` / `ctx.trace` /
/// `ctx.cron` (and therefore the auto-derived `ctx.query_api`).
///
/// Argv mode skips these by default to keep `baybo skills list` /
/// `baybo config get` boots cheap; this predicate opts the monitoring
/// surface back in. `Status { live: false }` stays out — only the
/// `--live` block needs the live counters.
///
/// Each manager built here uses only storage handles already opened
/// via `baybo_storage::Store::open`. No actors, supervisors, or LLM
/// dependencies — pure read-side wiring.
fn needs_query_graph(cmd: &Commands) -> bool {
    match cmd {
        Commands::Status { live } => *live,
        Commands::Cost { .. }
        | Commands::Session { .. }
        | Commands::Turn { .. }
        | Commands::Cron { .. } => true,
        _ => false,
    }
}

fn pick_format(cli: &Cli) -> OutputFormat {
    if cli.global.json {
        OutputFormat::Json
    } else if cli.global.plain {
        OutputFormat::Plain
    } else {
        OutputFormat::Human
    }
}

/// Resolve the effective `baybo prompt` text: the positional argument,
/// optionally merged with piped stdin. `baybo prompt` with no argument
/// reads the prompt entirely from stdin (`cat task.md | baybo prompt`); an
/// argument *plus* piped stdin appends the stdin as extra context
/// (`git diff | baybo prompt "review this"`). Stdin is read only when it
/// isn't a terminal, so an interactive `baybo prompt` with no argument
/// can't hang waiting on a human.
fn resolve_prompt(arg: String) -> anyhow::Result<String> {
    let mut prompt = arg;
    if !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        std::io::stdin().lock().read_to_string(&mut piped)?;
        let piped = piped.trim_end();
        if !piped.is_empty() {
            if prompt.trim().is_empty() {
                prompt = piped.to_owned();
            } else {
                prompt = format!("{prompt}\n\n{piped}");
            }
        }
    }
    if prompt.trim().is_empty() {
        anyhow::bail!(
            "no prompt provided — pass it as an argument (`baybo prompt \"...\"`) or pipe it via stdin"
        );
    }
    Ok(prompt)
}

/// Emit a shell completion script without running the rest of the boot chain.
fn print_completion(shell: ShellKind) -> anyhow::Result<()> {
    let out = baybo_cli::completion_script(shell).map_err(|e| anyhow::anyhow!(e))?;
    let rendered = out.render(OutputFormat::Plain);
    print!("{rendered}");
    Ok(())
}
