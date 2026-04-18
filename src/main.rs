mod boot;
mod gateway_cmd;
mod runtime;
mod singleton;
mod tui_cmd;
mod tui_log;

use aura_channels::TuiLogSink;
use aura_cli::cli::ShellKind;
use aura_cli::{Cli, Commands, ContextBuilder, Invocation, OutputFormat, dispatch};
use aura_security::{LeakDetector, RedactingMakeWriter};
use clap::CommandFactory;
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::runtime::build_leak_detector;
use crate::tui_log::TuiLogLayer;

struct SecondPrecisionTimer;

impl FormatTime for SecondPrecisionTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"))
    }
}

enum TracingMode<'a> {
    /// argv / one-shot command path: everything to stdout, no TUI echo.
    Stdout,
    /// Chat path: fmt layer writes rolling file under `<log_dir>/aura.log`,
    /// plus a warn/error echo layer feeding the TUI scrollback via the
    /// returned `OnceLock<TuiLogSink>`. The file writer is wrapped in a
    /// leak-detector redactor so secrets matched by `LeakAction::Replace`
    /// rules are masked before landing on disk.
    Chat {
        log_dir: &'a Path,
        leak_detector: Arc<LeakDetector>,
    },
}

pub struct ChatTracing {
    _file_guard: WorkerGuard,
    pub tui_sink: Arc<OnceLock<TuiLogSink>>,
}

fn init_tracing(mode: TracingMode<'_>) -> Option<ChatTracing> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aura=info"));
    let json = std::env::var("AURA_LOG_FORMAT").unwrap_or_default() == "json";

    match mode {
        TracingMode::Stdout => {
            let fmt_layer = fmt::layer()
                .with_timer(SecondPrecisionTimer)
                .with_target(true)
                .with_file(true)
                .with_line_number(true);
            if json {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer.json().with_span_list(true))
                    .init();
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer)
                    .init();
            }
            None
        }
        TracingMode::Chat {
            log_dir,
            leak_detector,
        } => {
            if let Err(e) = std::fs::create_dir_all(log_dir) {
                eprintln!(
                    "warning: could not create log dir {}: {e}. Falling back to stdout logging.",
                    log_dir.display()
                );
                return init_tracing(TracingMode::Stdout);
            }
            let appender = tracing_appender::rolling::daily(log_dir, "aura.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let writer = RedactingMakeWriter::new(leak_detector, writer);
            let tui_sink: Arc<OnceLock<TuiLogSink>> = Arc::new(OnceLock::new());
            let tui_layer = TuiLogLayer::new(Arc::clone(&tui_sink));
            let fmt_layer = fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_timer(SecondPrecisionTimer)
                .with_target(true)
                .with_file(true)
                .with_line_number(true);
            if json {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer.json().with_span_list(true))
                    .with(tui_layer)
                    .init();
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer)
                    .with(tui_layer)
                    .init();
            }
            Some(ChatTracing {
                _file_guard: guard,
                tui_sink,
            })
        }
    }
}

/// Resolve the effective aura.json path, if any, for display in diagnostics.
fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("AURA_CONFIG_PATH") {
        return Some(PathBuf::from(explicit));
    }
    let default = PathBuf::from("aura.json");
    default.exists().then_some(default)
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
            std::env::set_var("AURA_CONFIG_PATH", path);
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
    let workspace_root = PathBuf::from(&config.workspace.path);

    // TUI: ratatui owns stdout, so tracing goes to a rolling file under
    // `<workspace>/logs/` (redacted through the shared `LeakDetector`)
    // and warn/error events mirror into the chat scrollback via
    // `TuiLogLayer`. Gets its own early return so the rest of `main`
    // doesn't need to branch on chat_mode.
    if matches!(cli.command, Some(Commands::Tui)) {
        let log_dir = workspace_root.join("logs");
        let leak_detector = build_leak_detector(&config.security, None);
        let chat_tracing = init_tracing(TracingMode::Chat {
            log_dir: &log_dir,
            leak_detector: Arc::clone(&leak_detector),
        });
        info!("Aura - Intelligent Assistant Framework starting");
        return tui_cmd::run(config, leak_detector, chat_tracing).await;
    }

    // ---------------- argv dispatch (one-shot command + exit) ----------------
    //
    // Everything reaching this point is a one-shot argv command (Tui,
    // Completion, Gateway, None are all handled above). Argv mode needs
    // only the lightweight inspection set (skills, tools, channels,
    // workspace, optional LLM) — building the whole manager graph for
    // `aura status` would needlessly open libsql and recover jobs.
    init_tracing(TracingMode::Stdout);

    let cmd = cli.command.expect("non-command branches handled above");

    let skill_registry = {
        let reg = Arc::new(aura_skills::SkillRegistry::new());
        let workspace_skills = workspace_root.join("skills");
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
    let tool_registry = Arc::new(aura_tools::ToolRegistry::with_defaults());
    let workspace = Arc::new(aura_workspace::WorkspaceManager::new(
        workspace_root.clone(),
    ));
    let channels_registry = Arc::new(tokio::sync::RwLock::new(
        aura_channels::ChannelRegistry::new(),
    ));
    let llm_client = match boot::build_llm_client(&config.llm) {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            tracing::warn!(error = %e, "LLM client unavailable for this command");
            None
        }
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
