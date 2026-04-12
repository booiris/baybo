use clap::{Parser, Subcommand, ValueEnum};

/// Aura operator CLI.
///
/// Every subcommand is also reachable from inside an Aura chat session by
/// typing the same tokens with a leading `/`.
#[derive(Debug, Parser)]
#[command(name = "aura", version, about = "Aura operator CLI", long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Global flags shared by every subcommand.
#[derive(Debug, clap::Args, Default, Clone)]
pub struct GlobalArgs {
    /// Override AURA_CONFIG_PATH.
    #[arg(long, global = true, env = "AURA_CONFIG_PATH")]
    pub config: Option<String>,

    /// Emit machine-readable JSON on stdout. Disables color.
    #[arg(long, global = true)]
    pub json: bool,

    /// Plain text output (no color, no tables). Used when rendering to
    /// non-terminal channels.
    #[arg(long, global = true)]
    pub plain: bool,

    /// Disable ANSI color in human output.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Increase log verbosity. Repeat for trace-level logs.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// Top-level command families.
///
/// Only command families with backing manager APIs are exposed here. Other
/// families listed in `docs/modules/cli.md` are added as their subsystems
/// gain public read/write methods.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Inspect and edit the Aura configuration.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// List and inspect registered skills.
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// List and inspect registered tools.
    Tools {
        #[command(subcommand)]
        cmd: ToolsCmd,
    },
    /// Inspect channel adapters.
    Channels {
        #[command(subcommand)]
        cmd: ChannelsCmd,
    },
    /// LLM provider and model status.
    Llm {
        #[command(subcommand)]
        cmd: LlmCmd,
    },
    /// Workspace identity and layout.
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// One-shot summary of current runtime state.
    Status,
    /// Run health checks against config, storage, and env.
    Doctor,
    /// Emit shell completion script.
    Completion {
        #[arg(value_enum)]
        shell: ShellKind,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print the full config or one section as JSON.
    Show {
        /// Section name (e.g. `llm`, `agent`, `security`). Omit for the whole config.
        section: Option<String>,
    },
    /// Validate a config file on disk.
    Validate {
        /// Path to the config file. Defaults to the resolved config path.
        #[arg(long)]
        file: Option<String>,
    },
    /// Print the path of the active config file.
    File,
    /// Print a JSON snapshot of the default config as a schema reference.
    Schema,
}

#[derive(Debug, Subcommand)]
pub enum SkillsCmd {
    /// List registered skill names.
    List,
    /// Show a skill's metadata.
    Info {
        /// Skill name.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ToolsCmd {
    /// List tools visible to the LLM.
    List,
    /// Show a tool's manifest and parameter schema.
    Info {
        /// Tool name.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChannelsCmd {
    /// List registered channel adapters and their current status.
    List,
}

#[derive(Debug, Subcommand)]
pub enum LlmCmd {
    /// Show the configured LLM provider, model id, and capabilities.
    Status,
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCmd {
    /// Show the workspace root and loaded identity files.
    Show,
}

#[derive(Debug, Copy, Clone, ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
}
