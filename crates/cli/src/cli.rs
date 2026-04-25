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
///
/// `--config` is pure UX sugar: `main` pushes its value into
/// `AURA_CONFIG_PATH` once at startup, and every downstream reader goes
/// through the env var. One source of truth at the read site, two
/// surfaces at the call site.
#[derive(Debug, clap::Args, Default, Clone)]
pub struct GlobalArgs {
    /// Path to the Aura config file. Overrides `AURA_CONFIG_PATH`.
    /// Clap's `env = "..."` makes the env var the fallback so both
    /// surfaces land in the same field.
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
    /// Inspect channel adapters.
    #[command(name = "channel")]
    Channel {
        #[command(subcommand)]
        cmd: ChannelCmd,
    },
    /// Manage Model Context Protocol servers exposed to the agent loop.
    /// MCP-discovered tools surface to the LLM as `<server>/<tool>` and
    /// are persisted to `.mcp.json` at the workspace root.
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Manage per-user pairings: approve new senders, list pending
    /// requests, revoke existing approvals. See
    /// `docs/modules/pairing.md`.
    Pair {
        #[command(subcommand)]
        cmd: PairCmd,
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
    /// Send a one-shot message to the agent.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Launch the interactive Ratatui chat session.
    ///
    /// Connects to an `aura gateway` over HTTP+SSE. The endpoint is
    /// derived from `config.gateway.{bind_address,port}` and the
    /// bearer token is read from the workspace vault. If the gateway
    /// is unreachable the command exits with an error block
    /// describing how to start one.
    Tui {
        /// Resume an existing session by id instead of creating a new
        /// one. Sessions created with a different channel (http) are
        /// acceptable; the TUI just pins to that id.
        #[arg(long, value_name = "SESSION_ID")]
        session: Option<String>,
        /// Dev-only: if the gateway is unreachable, spawn one as a
        /// subprocess for the duration of this TUI session. Gated on
        /// `debug_assertions` so release builds can never expose it.
        #[cfg(debug_assertions)]
        #[arg(long)]
        dev_auto_gateway: bool,
    },
    /// Run or manage the HTTP gateway service.
    Gateway {
        #[command(subcommand)]
        cmd: GatewayCmd,
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
    /// Case-insensitive substring search over name, description, and
    /// command triggers. Omit `query` to return every skill.
    Search {
        /// Substring to match.
        query: Option<String>,
    },
    /// Validate skills against their declared requirements
    /// (required binaries on `$PATH`, required env vars set, declarative
    /// shape). With `name`, check one skill; without, check all.
    Check {
        /// Optional skill name. Omit to check every skill.
        name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChannelCmd {
    /// List every registered bot across all channels — one row per
    /// bot, showing the bot id and its channel.
    List,
    /// Register a new bot. Opens an interactive single-select over the
    /// supported channel types, then dispatches into that channel's
    /// registration flow (e.g. telegram prompts once for a masked bot
    /// token). Writes directly to libsql + the vault; a running
    /// gateway picks up the new bot within a couple of seconds via
    /// the reconciler.
    Add,
    /// Deregister a bot. Opens an interactive single-select over every
    /// registered bot (showing bot id + channel), then asks for y/N
    /// confirmation. Soft-deletes the row and removes its vault
    /// secret; a running gateway's reconciler pushes a `StopBot` to
    /// the sidecar on the next tick.
    Remove,
}

#[derive(Debug, Subcommand)]
pub enum McpCmd {
    /// Register a Model Context Protocol server. For HTTP transports
    /// guarded by OAuth, the authorization flow runs inline before the
    /// entry is persisted; failed auth → no mutation.
    ///
    /// Mirrors `claude mcp add`. The trailing positional after `name` is
    /// the URL (HTTP) or the binary path (stdio); any further positionals
    /// after `--` are forwarded as args to the stdio command.
    #[command(after_help = "\
EXAMPLES:
  # HTTP server (no auth)
  aura mcp add --transport http sentry https://mcp.sentry.dev/mcp

  # HTTP server with a static bearer token (stored encrypted in the vault)
  aura mcp add --transport http corridor https://app.corridor.dev/api/mcp \\
      --header \"Authorization: Bearer abc123\"

  # HTTP server with OAuth — runs the browser-based authorization flow at add-time;
  # tokens are persisted only on success
  aura mcp add --transport http github https://api.githubcopilot.com/mcp/ \\
      --client-id Iv23liABCDEF

  # HTTP server with OAuth, providing a client secret (read from stdin)
  aura mcp add --transport http figma https://mcp.figma.com/mcp \\
      --client-id <id> --client-secret

  # HTTP server with OAuth on a fixed callback port (for pre-registered redirect URIs)
  aura mcp add --transport http acme https://mcp.acme.dev/mcp \\
      --client-id <id> --callback-port 8765

  # stdio server with env vars
  aura mcp add -e API_KEY=xxx my-server -- npx my-mcp-server

  # stdio server with subprocess flags
  aura mcp add my-server -- my-command --some-flag arg1

  # Trust level
  aura mcp add --transport http trusted-srv https://mcp.example.com/mcp \\
      --trust-level trusted

NOTES:
  * Stdio servers ignore --header/--client-id/--client-secret/--callback-port.
  * HTTP servers ignore --env (use --header for auth headers).
  * On success the running gateway picks up the new tools within ~5 seconds via the
    MCP reconciler — no restart required.
")]
    Add {
        /// Transport type. Defaults to `stdio` if absent.
        #[arg(short = 't', long, value_enum, default_value_t = McpTransportArg::Stdio)]
        transport: McpTransportArg,
        /// Environment variable to inject into a stdio MCP server, in
        /// `KEY=VALUE` form. Repeatable.
        #[arg(short = 'e', long = "env")]
        envs: Vec<String>,
        /// Header to attach to every HTTP request, in `Header: Value`
        /// form. Repeatable. Stored encrypted in the secret vault.
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// Pre-registered OAuth client id. If absent and the server
        /// requires OAuth, Aura attempts Dynamic Client Registration.
        #[arg(long)]
        client_id: Option<String>,
        /// Prompt for the OAuth client secret (read from stdin).
        #[arg(long)]
        client_secret: bool,
        /// Fixed callback port for the OAuth redirect URI. Use this when
        /// the OAuth provider requires a pre-registered redirect URI;
        /// otherwise an ephemeral port is bound.
        #[arg(long)]
        callback_port: Option<u16>,
        /// Trust ceiling applied to every tool the server exports.
        /// Defaults to `installed`.
        #[arg(long, value_enum, default_value_t = TrustLevelArg::Installed)]
        trust_level: TrustLevelArg,
        /// Server identifier. Must be unique within `.mcp.json`. Used
        /// as the prefix for tool names exposed to the LLM
        /// (`<name>/<tool>`).
        name: String,
        /// For stdio: the binary path or `npx`-style entry. For HTTP:
        /// the server URL.
        command_or_url: String,
        /// Trailing args passed to a stdio command (after `--`). Ignored
        /// for HTTP transports.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// List configured MCP servers across `.mcp.json`. Each row probes
    /// the server with a short timeout and reports a STATUS column.
    /// Pass `--no-probe` for config-only output (cheap, no subprocess
    /// spawn / HTTP roundtrip).
    List {
        /// Skip the live connection probe; render config only.
        #[arg(long)]
        no_probe: bool,
    },
    /// Show one server's full record. Vault values are rendered as
    /// `********`. By default the server is probed; pass `--no-probe`
    /// for config-only output.
    Get {
        /// Server name.
        name: String,
        /// Skip the live connection probe; render config only.
        #[arg(long)]
        no_probe: bool,
    },
    /// Remove an MCP server. Drops the entry from `.mcp.json` and every
    /// associated vault key. Requires `--yes` in slash mode.
    Remove {
        /// Server name.
        name: String,
        /// Confirm the destructive action (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Copy, Clone, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum McpTransportArg {
    Stdio,
    Http,
    /// Accepted for `claude mcp add` parity; treated as `http`.
    Sse,
}

#[derive(Debug, Copy, Clone, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum TrustLevelArg {
    Trusted,
    Installed,
    Untrusted,
}

#[derive(Debug, Subcommand)]
pub enum PairCmd {
    /// List live pairings. With no flag, shows every row (pending,
    /// expired, approved). Pass `--pending` or `--approved` to
    /// restrict.
    List {
        /// Show only pending rows (including expired).
        #[arg(long, conflicts_with = "approved")]
        pending: bool,
        /// Show only approved rows.
        #[arg(long)]
        approved: bool,
    },
    /// Approve a pending pairing by its short code. The code is
    /// surfaced to the end-user as a chat notice the first time
    /// they message an un-paired bot.
    Approve {
        /// Short pairing code (6 chars, unambiguous alphabet).
        code: String,
    },
    /// Soft-delete an approved or pending pairing. Subsequent
    /// messages from the triple trigger a fresh pending row with a
    /// fresh code.
    Revoke {
        /// Channel type, e.g. `telegram`.
        channel_type: String,
        /// Bot id the pairing is scoped to. Pass `""` for channels
        /// with no bot concept.
        bot_id: String,
        /// Platform user id (`tg_<botId>_<chatId>_<userId>` for
        /// Telegram, matching the sidecar's composed id).
        user_id: String,
        /// Confirm the destructive action (required in slash mode).
        #[arg(long, short = 'y')]
        yes: bool,
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
    /// List scheduled cron jobs across every user. Operator read-only view;
    /// all mutations (create/delete/enable/run) are driven through the LLM
    /// tools (`CronCreate`, `CronDelete`, …).
    List,
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
    /// Print the nearest stored context snapshot for a trace node. Walks
    /// the ancestor chain from `--node` (defaults to `active_leaf`) and
    /// returns the first node carrying a `context_snapshot`. Read-only:
    /// this does **not** take a new live snapshot (live-snapshot capture
    /// still requires the session context the CLI does not hold).
    Snapshot {
        /// Session id whose trace to inspect.
        id: String,
        /// Trace node id to start the lookup from. Defaults to the
        /// session's `active_leaf`.
        #[arg(long)]
        node: Option<String>,
        /// Include the full message bodies in the response. Off by
        /// default — a summary (role + token count + message count) is
        /// usually enough for operators.
        #[arg(long)]
        full: bool,
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

#[derive(Debug, Subcommand)]
pub enum AgentCmd {
    /// Send a one-shot message to a session's agent loop. Forbidden inside
    /// a chat session (returns `AgentSendForbiddenInSlash`). In argv mode
    /// this requires the agent runtime to expose a synchronous one-shot
    /// entry point, which is still deferred pending daemon/RPC work.
    Send {
        /// Session id the message should be appended to.
        #[arg(long)]
        session: String,
        /// Literal message content sent as a user turn.
        #[arg(long)]
        message: String,
        /// Confirm the mutation. Slash mode rejects before this is read.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum GatewayCmd {
    /// Run the HTTP gateway in the foreground.
    Start,
    /// Write a platform service unit (systemd user unit on Linux,
    /// launchd agent on macOS) pointing at the current binary.
    Install {
        /// Install a system-wide unit instead of a per-user unit.
        /// Requires root on Linux.
        #[arg(long)]
        system: bool,
        /// Override the `ExecStart` path. Use this when the release
        /// binary lives somewhere other than `$PATH`.
        #[arg(long)]
        exec_start: Option<String>,
    },
    /// Mint the auth token if absent and enable autostart at boot.
    Enable,
    /// Disable autostart at boot. Leaves the unit file in place.
    Disable,
    /// Remove the service unit. The vault-stored token is left in
    /// place — use `token rotate` to invalidate a leaked one.
    Uninstall {
        /// Confirm the mutation in slash mode.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Print installation and runtime status of the service.
    Status,
    /// Inspect or rotate the auth token.
    Token {
        #[command(subcommand)]
        cmd: GatewayTokenCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum GatewayTokenCmd {
    /// Print the current auth token.
    Show,
    /// Replace the token with a newly generated one and print it.
    /// Requires `--yes` in slash mode.
    Rotate {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Regression guard for the dev-only `--dev-auto-gateway` flag.
    /// Release builds must never expose it — it spawns a subprocess
    /// with no operator supervision, which is acceptable in a dev
    /// workflow and dangerous in production. Gated on
    /// `debug_assertions`, so release builds must not carry it.
    #[test]
    #[cfg(not(debug_assertions))]
    fn tui_has_no_dev_auto_gateway_flag_in_release() {
        let cmd = Cli::command();
        let tui = cmd.find_subcommand("tui").expect("tui subcommand");
        for arg in tui.get_arguments() {
            assert_ne!(
                arg.get_id().as_str(),
                "dev_auto_gateway",
                "--dev-auto-gateway leaked into a release build"
            );
        }
    }

    /// Mirror test: in debug builds the flag must be present. Keeps
    /// the positive case honest.
    #[test]
    #[cfg(debug_assertions)]
    fn tui_has_dev_auto_gateway_flag_in_debug() {
        let cmd = Cli::command();
        let tui = cmd.find_subcommand("tui").expect("tui subcommand");
        assert!(
            tui.get_arguments()
                .any(|a| a.get_id().as_str() == "dev_auto_gateway"),
            "--dev-auto-gateway missing from debug build"
        );
    }
}
