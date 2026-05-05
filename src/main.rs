mod boot;
mod gateway_cmd;
mod runtime;
mod setup_cmd;
mod singleton;
mod tracing_init;
mod tui_cmd;
mod tui_log;

use aura_cli::cli::ShellKind;
use aura_cli::{Cli, Commands, ContextBuilder, Invocation, OutputFormat, dispatch};
use clap::CommandFactory;
use clap::Parser;
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
    let cli = Cli::parse();

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
    // chat loop is reached via the explicit `aura tui` subcommand so the
    // default invocation doesn't surprise users with a full-screen app.
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

    let skill_registry = {
        let reg = Arc::new(aura_skills::SkillRegistry::new());
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
    };
    let stores = aura_storage::Store::open(boot::storage_db_path(&config.workspace)).await?;
    // Argv-mode commands (`llm probe`, `doctor`, `status`, `channel add`,
    // …) don't drive WebFetch through an agent loop, so wiring a side
    // LLM here would just be paperwork. Keep it `None` and let the few
    // boot paths that *do* use tools (gateway/runtime) opt in.
    let tool_registry = Arc::new(aura_tools::ToolRegistry::with_defaults(
        stores.blob.clone(),
        None,
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
        match boot::build_llm_client(&config, None, None).await {
            Ok(c) => Some(Arc::new(c)),
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
    matches!(cmd, Commands::Doctor | Commands::Status)
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

/// Emit a shell completion script without running the rest of the boot chain.
fn print_completion(shell: ShellKind) -> anyhow::Result<()> {
    let out = aura_cli::completion_script(shell).map_err(|e| anyhow::anyhow!(e))?;
    let rendered = out.render(OutputFormat::Plain);
    print!("{rendered}");
    Ok(())
}
