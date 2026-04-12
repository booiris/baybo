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
    /// Security audit and leak scanning.
    Security {
        #[command(subcommand)]
        cmd: SecurityCmd,
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
    /// Inspect and manage chat sessions.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Inspect and cancel tracked jobs.
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Inspect and manage cron-scheduled jobs.
    Cron {
        #[command(subcommand)]
        cmd: CronCmd,
    },
    /// Inspect and manage stored user memories.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Inspect session traces.
    Trace {
        #[command(subcommand)]
        cmd: TraceCmd,
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
    /// Print a single config value by path (e.g. `llm.model` or `/llm/model`).
    Get {
        /// Dotted or JSON-pointer path.
        path: String,
    },
    /// Write a new value at `path` and persist the config. Requires `--yes` in
    /// slash mode. Reload requires restart until hot-reload ships.
    Set {
        /// Dotted or JSON-pointer path.
        path: String,
        /// JSON literal. Unquoted strings (not valid JSON) are accepted verbatim.
        value: String,
        /// Confirm the write (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Remove the value at `path` (resets to default) and persist the config.
    /// Requires `--yes` in slash mode.
    Unset {
        /// Dotted or JSON-pointer path.
        path: String,
        /// Confirm the write (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
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
    /// Execute a tool outside an agent turn, for operator testing. The attempt
    /// is recorded in trace + cost + job ledgers like a real call. Requires
    /// `--yes` under slash mode.
    Test {
        /// Tool name.
        name: String,
        /// JSON object passed as tool parameters (default: `{}`).
        #[arg(long, default_value = "{}")]
        args: String,
        /// Confirm the execution (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChannelsCmd {
    /// List registered channel adapters and their current status.
    List,
}

#[derive(Debug, Subcommand)]
pub enum SecurityCmd {
    /// Print a structured view of the gateway's active posture
    /// (leak rules, secret vault presence).
    Audit,
    /// Leak-detector operations.
    Leaks {
        #[command(subcommand)]
        cmd: LeaksCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum LeaksCmd {
    /// Scan a file on disk for known secret patterns.
    Check {
        /// Path to the file to scan.
        path: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum LlmCmd {
    /// Show the configured LLM provider, model id, and capabilities.
    Status,
    /// List the model catalog each registered provider advertises.
    Models,
    /// Send a one-token chat request to the configured provider to verify
    /// connectivity and auth. Feeds `aura doctor`.
    Probe,
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCmd {
    /// Show the workspace root and loaded identity files.
    Show,
    /// Overwrite one of the four workspace identity documents
    /// (`AGENTS.md` / `SOUL.md` / `USER.md` / `IDENTITY.md`). Requires
    /// `--yes` in slash mode. Change picks up after restart.
    SetIdentity {
        /// Which identity file to write: `agents`, `soul`, `user`, or `identity`.
        name: String,
        /// Path to a file whose contents replace the identity document.
        /// Mutually exclusive with `--content`.
        #[arg(long, conflicts_with = "content")]
        file: Option<String>,
        /// Literal content to write. Mutually exclusive with `--file`.
        #[arg(long)]
        content: Option<String>,
        /// Confirm the write (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionCmd {
    /// List known sessions, newest-active first.
    List,
    /// Show session metadata.
    Show {
        /// Session id.
        id: String,
    },
    /// Show the chat transcript for a session.
    History {
        /// Session id.
        id: String,
    },
    /// Delete a session and its transcript. Requires `--yes` in slash mode.
    Kill {
        /// Session id.
        id: String,
        /// Confirm the destructive action (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum JobCmd {
    /// List tracked jobs, optionally filtered by status.
    List {
        /// Filter by status: pending, in-progress, completed, submitted,
        /// accepted, failed, stuck.
        #[arg(long)]
        status: Option<JobStatusArg>,
    },
    /// Show a job's metadata.
    Show {
        /// Job id.
        id: String,
    },
    /// Cancel a running or pending job. Requires `--yes` in slash mode.
    Cancel {
        /// Job id.
        id: String,
        /// Confirm the destructive action (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

/// Status filter accepted by `aura job list --status`.
#[derive(Debug, Copy, Clone, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum JobStatusArg {
    Pending,
    InProgress,
    Completed,
    Submitted,
    Accepted,
    Failed,
    Stuck,
}

#[derive(Debug, Subcommand)]
pub enum CronCmd {
    /// List scheduled cron jobs across every user. Operator view.
    List,
    /// Show a cron job's metadata.
    Show {
        /// Cron job id.
        id: String,
    },
    /// Remove a cron job. Requires `--yes` in slash mode.
    Rm {
        /// Cron job id.
        id: String,
        /// Confirm the destructive action (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Enable a previously-disabled cron job.
    Enable {
        /// Cron job id.
        id: String,
    },
    /// Disable a cron job, clearing its next trigger time.
    Disable {
        /// Cron job id.
        id: String,
    },
    /// Manually fire a cron job now, outside the regular schedule. Requires
    /// `--yes` in slash mode.
    Run {
        /// Cron job id.
        id: String,
        /// Confirm the side-effect (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List execution records for a cron job.
    Runs {
        /// Cron job id whose runs to list.
        #[arg(long)]
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum MemoryCmd {
    /// List stored memories. Omit `--user` for an operator-wide view.
    List {
        /// Scope results to a specific user.
        #[arg(long, short = 'u')]
        user: Option<String>,
        /// Cap the number of entries returned (default: 50).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Substring-search memory contents. Omit `--user` for a global scan.
    Search {
        /// Query string matched against content (case-insensitive substring).
        query: String,
        /// Scope results to a specific user.
        #[arg(long, short = 'u')]
        user: Option<String>,
        /// Cap the number of hits returned (default: 20).
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show a memory entry by id.
    Show {
        /// Memory entry id.
        id: String,
    },
    /// Raise a memory entry's importance. Requires `--yes` in slash mode.
    Promote {
        /// Memory entry id.
        id: String,
        /// New importance in `[0.0, 1.0]`. Defaults to 1.0 (pin).
        #[arg(long, default_value_t = 1.0)]
        to: f32,
        /// Confirm the write (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Delete every memory entry recorded from a given session. Requires
    /// `--yes` in slash mode.
    Clear {
        /// Session id whose memories should be purged.
        #[arg(long)]
        session: String,
        /// Confirm the destructive action (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum TraceCmd {
    /// List stored session traces (newest first).
    List {
        /// Scope to a specific session id.
        #[arg(long)]
        session: Option<String>,
        /// Cap the number of rows returned (default: 50).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show a session's trace tree summary.
    Show {
        /// Session id whose trace to inspect.
        id: String,
    },
    /// Export a session's trace as pretty JSON. Prints to stdout unless
    /// `--out <path>` is given, which writes the file in argv mode. Requires
    /// `--yes` under slash mode because the write path is operator-controlled.
    Export {
        /// Session id whose trace to export.
        id: String,
        /// Optional output file path. Without it, JSON is returned in the
        /// command output.
        #[arg(long)]
        out: Option<String>,
        /// Confirm the file write (required in slash mode when `--out` is
        /// set).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Copy, Clone, ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Powershell,
    Elvish,
}
