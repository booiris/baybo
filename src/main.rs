mod boot;
mod gateway_client;
mod gateway_cmd;
mod prompt_cmd;
mod reload;
mod runtime;
mod setup_cmd;
mod singleton;
mod tracing_init;
mod tui_cmd;
mod tui_log;

use aura_cli::cli::ShellKind;
use aura_cli::{Cli, Commands, ContextBuilder, Invocation, OutputFormat, dispatch};
use clap::CommandFactory;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

use crate::tracing_init::{TracingMode, init_tracing};

/// Resolve the effective aura.json path, if any, for display in
/// diagnostics. Thin wrapper so existing callers keep working; the real
/// implementation lives in `boot` so `gateway_cmd` can reuse it without
/// depending on `main.rs`.
fn resolve_config_path() -> Option<PathBuf> {
    boot::resolve_config_path()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `parse_args` checks `AURA_HELP_AGENT` and swaps in an unhidden
    // `Command` before clap parses argv; that's how the env-var
    // surfaces extended `--help` output without needing a flag.
    let cli = aura_cli::cli::parse_args();

    // Promote `--config <path>` into `AURA_CONFIG_PATH` before any
    // reader runs, so every downstream caller can stay env-only with a
    // single source of truth.
    //
    // SAFETY: `setenv` is process-global; Rust 2024 marks it unsafe
    // because a concurrent `getenv` on another thread can observe a
    // torn read on some libc implementations. We write exactly once,
    // exactly here, before `boot::load_config` (the only reader in this
    // binary) runs, and no other code in the process mutates or reads
    // `AURA_CONFIG_PATH` concurrently — so the race window is empty in
    // practice.
    if let Some(path) = cli.global.config.as_deref() {
        unsafe {
            std::env::set_var(aura_workspace::paths::ENV_CONFIG_PATH, path);
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
    // master key + default aura.json before any of the normal
    // `boot::load_config` machinery can run, so it gets its own
    // entry point here, ahead of the argv/chat dispatch.
    if let Some(Commands::Setup) = cli.command.as_ref() {
        return setup_cmd::run().await;
    }

    // Bare `aura` (no subcommand) prints help and exits. The interactive
    // chat loop is reached via the explicit `aura tui` subcommand, and
    // one-shot answering via `aura prompt`, so the default invocation
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
        aura_workspace::WorkspacePaths::new(PathBuf::from(&config.workspace.path));
    let workspace_root = workspace_paths.root().to_path_buf();
    aura_workspace::WorkspaceManager::new(workspace_root.clone())
        .ensure_layout()
        .await?;

    // One-shot `aura prompt`: stream a single answer and exit. Sits
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
    // tool metadata, but avoids the full manager graph and job recovery.
    init_tracing(TracingMode::Stdout);

    let cmd = cli.command.expect("non-command branches handled above");

    let skill_registry = if needs_skills(&cmd) {
        let reg = Arc::new(aura_skills::SkillRegistry::new());
        let builtins = reg.register_builtins();
        if builtins > 0 {
            info!(count = builtins, "registered built-in skills");
        }
        let workspace_skills = workspace_paths.skills_dir();
        let loaded = reg.load_dir(&workspace_skills);
        if loaded > 0 {
            info!(
                count = loaded,
                path = %workspace_skills.display(),
                "loaded skills from workspace"
            );
        }
        reg
    } else {
        Arc::new(aura_skills::SkillRegistry::new())
    };
    let stores = aura_storage::Store::open(boot::storage_db_path(&config.workspace)).await?;
    // Argv-mode commands (`llm probe`, `doctor`, `status`, `channel add`,
    // …) don't drive WebFetch through an agent loop; the per-call
    // `ToolContext::llm` is left `None` in those paths, so WebFetch
    // silently falls back to raw markdown.
    let tool_proxy = boot::proxy_settings(&config)
        .as_ref()
        .map(|p| p.to_proxy())
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid proxy.url: {e}"))?;
    let tool_registry = Arc::new(aura_tools::ToolRegistry::with_defaults(
        stores.blob.clone(),
        workspace_paths.clone(),
        tool_proxy,
        false,
    ));
    let workspace = Arc::new(aura_workspace::WorkspaceManager::new(
        workspace_root.clone(),
    ));
    let channels_registry = Arc::new(aura_channels::ChannelRegistry::new());
    // Only `llm`, `doctor`, and `status` touch `ctx.llm` in the argv
    // path. Building the client unconditionally meant every run of
    // `aura channel add` / `aura config get` / etc. emitted a warn-level
    // "LLM client unavailable" message when no API key was configured,
    // which users reasonably interpreted as a hard error.
    let llm_client = if needs_llm(&cmd) {
        // `llm` / `doctor` / `status` never send multimodal content,
        // so it's fine to skip the BlobStore wiring here — opening
        // libsql for a status probe would be wasteful.
        // No vault here either: argv-mode `llm` / `doctor` / `status` are
        // probes that don't need OAuth tokens; the openai-subscription
        // provider's create() returns a clear error if it's selected
        // without a vault.
        let provider_registry = aura_llm::LlmProviderRegistry::with_default_providers();
        match boot::build_llm_client(
            &config,
            &provider_registry,
            None,
            None,
            aura_llm::CostHooks::passthrough(),
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
        // `session`, `job`, `cron`) need the heavier domain graph that
        // argv-mode skips by default. Build the smallest set that lets
        // those handlers (and the auto-derived `QueryApi`) work, without
        // dragging in actors, supervisors, or LLM-side dependencies.
        //
        // `CronScheduler` needs a trigger channel and a `Shutdown`, but
        // we never call `.run()` here — only its read APIs. The dropped
        // receiver is fine: nothing in argv would push a trigger anyway,
        // and `ShutdownSignal::new()` returns an un-fired signal.
        builder = builder
            .session(Arc::new(aura_agent::SessionManager::new(
                stores.session.clone(),
                stores.session_summary.clone(),
            )))
            .job(Arc::new(aura_job::JobLifecycle::new(stores.job.clone())))
            .trace(stores.trace.clone())
            .cost_store(stores.cost.clone());
        let (cron_tx, _cron_rx) = tokio::sync::mpsc::channel(1);
        let shutdown: Arc<dyn aura_cron::Shutdown> = Arc::new(aura_agent::ShutdownSignal::new());
        builder = builder.cron(Arc::new(aura_agent::CronScheduler::new(
            stores.cron.clone(),
            cron_tx,
            shutdown,
        )));
    }
    if let Ok((vault, stores)) = runtime::build_bot_registry_deps(&config).await {
        builder = builder
            .secret_vault(vault)
            .channel_bot_store(stores.channel_bot)
            .channel_pairing_store(stores.channel_pairing);
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
/// `aura llm` subcommands intentionally aren't here: their handlers
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
/// `aura config get`, `aura cost show`, etc.
fn needs_skills(cmd: &Commands) -> bool {
    matches!(cmd, Commands::Skills { .. } | Commands::Status { .. })
}

/// Subcommands that read `ctx.session` / `ctx.job` / `ctx.trace` /
/// `ctx.cron` (and therefore the auto-derived `ctx.query_api`).
///
/// Argv mode skips these by default to keep `aura skills list` /
/// `aura config get` boots cheap; this predicate opts the monitoring
/// surface back in. `Status { live: false }` stays out — only the
/// `--live` block needs the live counters.
///
/// Each manager built here uses only storage handles already opened
/// via `aura_storage::Store::open`. No actors, supervisors, or LLM
/// dependencies — pure read-side wiring.
fn needs_query_graph(cmd: &Commands) -> bool {
    match cmd {
        Commands::Status { live } => *live,
        Commands::Cost { .. }
        | Commands::Session { .. }
        | Commands::Job { .. }
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

/// Resolve the effective `aura prompt` text: the positional argument,
/// optionally merged with piped stdin. `aura prompt` with no argument
/// reads the prompt entirely from stdin (`cat task.md | aura prompt`); an
/// argument *plus* piped stdin appends the stdin as extra context
/// (`git diff | aura prompt "review this"`). Stdin is read only when it
/// isn't a terminal, so an interactive `aura prompt` with no argument
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
            "no prompt provided — pass it as an argument (`aura prompt \"...\"`) or pipe it via stdin"
        );
    }
    Ok(prompt)
}

/// Emit a shell completion script without running the rest of the boot chain.
fn print_completion(shell: ShellKind) -> anyhow::Result<()> {
    let out = aura_cli::completion_script(shell).map_err(|e| anyhow::anyhow!(e))?;
    let rendered = out.render(OutputFormat::Plain);
    print!("{rendered}");
    Ok(())
}
