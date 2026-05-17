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
    /// Clap's `env = ...` makes the env var the fallback so both
    /// surfaces land in the same field. The variable name comes from
    /// `aura_workspace::paths::ENV_CONFIG_PATH` so there is one source of
    /// truth across the codebase.
    #[arg(long, global = true, env = aura_workspace::paths::ENV_CONFIG_PATH)]
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
}

/// Environment variable that, when set to any non-empty value, makes
/// every CLI invocation surface the extended help (hidden subcommands
/// like `config`, `log`, `session`, `job`, `cron`, `cost` and the
/// `-v`/`--verbose` flag become visible in `aura --help`).
///
/// The agent-side `BashTool` exports this automatically for any
/// command containing the bin name, so the model sees the full help
/// inventory without composing an argv flag. Humans can opt in
/// per-shell with `export AURA_HELP_AGENT=1`.
pub const ENV_HELP_AGENT: &str = "AURA_HELP_AGENT";

/// Top-level command families.
///
/// Only command families with backing manager APIs are exposed here. Other
/// families listed in `docs/modules/cli.md` are added as their subsystems
/// gain public read/write methods.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Read and edit the active `aura.json`: `show`, `get`, `set`,
    /// `unset`, `validate`, `file`, `schema`. Mutations take effect
    /// after restart (hot-reload deferred).
    #[command(hide = true)]
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Inspect registered skills: `list`, `info <name>`, `search [q]`,
    /// `check [name]` (verify required bins / env / model declarations).
    /// Read-only.
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Manage channel bots (Telegram/Slack/Discord/…): `list` the
    /// registered bots, `add` an interactive registration flow,
    /// `remove` deregisters and cleans up the vault token.
    #[command(name = "channel")]
    Channel {
        #[command(subcommand)]
        cmd: ChannelCmd,
    },
    /// Manage Model Context Protocol servers (`add` / `list` / `get` /
    /// `remove`). Servers are persisted to `.mcp.json`; their tools
    /// surface to the agent as `<server>/<tool>` and the running
    /// gateway picks up changes within ~5s without restart.
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Manage per-user channel pairings (the approval gate that lets
    /// a new sender talk to the agent): `list` (pending/approved),
    /// `approve <code>`, `revoke <channel> <bot> <user>`.
    Pair {
        #[command(subcommand)]
        cmd: PairCmd,
    },
    /// Manage LLM provider entries in `aura.json`: `status` (which
    /// entries are registered), `probe [name]` (round-trip a tiny
    /// chat to verify auth + connectivity), `live-model [name]`
    /// (list the provider's catalog), and the interactive editors
    /// `add` / `edit` / `remove` / `default`.
    Llm {
        #[command(subcommand)]
        cmd: LlmCmd,
    },
    /// Inspect chat sessions: `list` all sessions, `show <id>` for
    /// metadata + (when wired) execution-trace counts, `history <id>`
    /// for the conversation transcript (with
    /// `--include-superseded` / `--superseded-only` to surface
    /// compaction-dropped turns), and `export <id> [--out]` for the
    /// full LLM/tool call tree as JSON.
    #[command(hide = true)]
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Inspect tracked async jobs: `list [--status]`, `show <id>`,
    /// `cancel <id>` (requires `--yes` in slash mode). Jobs are the
    /// per-turn unit of work — one job per agent loop invocation.
    #[command(hide = true)]
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Inspect cron-scheduled jobs: `list` all schedules, `show <id>`
    /// for the full row including prompt body. Cron mutations
    /// (create/delete/enable/disable/run) are LLM-only via the
    /// `CronCreate` / `CronDelete` / `CronList` tools.
    #[command(hide = true)]
    Cron {
        #[command(subcommand)]
        cmd: CronCmd,
    },
    /// Read the rolling tracing log files on disk. `main` reads
    /// `<workspace>/logs/aura.log.<date>` (gateway/agent output);
    /// `channel <type>` reads
    /// `<workspace>/logs/channel/<type>.log.<date>` (sidecar output).
    /// Both tail the last `-n` lines (default 200) and `--follow`
    /// streams new lines until Ctrl-C. For structured session
    /// traces (LLM calls, tool calls) use `aura session export`
    /// instead — different store, different read shape.
    #[command(hide = true)]
    Log {
        #[command(subcommand)]
        cmd: LogCmd,
    },
    /// Send a one-shot message to an existing session's agent loop
    /// (`send --session <id> --message <text>`). Disabled inside a
    /// chat session — running it from slash mode would inject a turn
    /// into the session that's running it.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Inspect LLM spend (USD + token counts) recorded by the cost
    /// manager. `show` aggregates over a scope — `--user <id>`,
    /// `--session <id>`, `--job <id>`, or the default current-UTC-day
    /// range bounded by `--since` / `--until`. Scopes are mutually
    /// exclusive.
    #[command(hide = true)]
    Cost {
        #[command(subcommand)]
        cmd: CostCmd,
    },
    /// Launch the interactive Ratatui chat session.
    ///
    /// Connects to an `aura gateway` over the `/v1/channel-ws`
    /// endpoint on its admin listener (same bind the admin REST API
    /// and the web chat page use). The address is read from
    /// `gateway.bind_address`/`gateway.port` in config, and the
    /// per-start TUI token comes from the workspace vault under
    /// `gateway.tui_token`. If the gateway is unreachable the
    /// command exits with an error block describing how to start one.
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
    /// One-shot snapshot of the running Aura instance.
    ///
    /// Default view is the *static* inventory: registered skills /
    /// tools / channels / current LLM model / config path — answers
    /// "how is this process configured". Pass `--live` to also query
    /// the live runtime: in-flight job count, failed jobs in the last
    /// 24h, and today's LLM spend (parallelized via `tokio::try_join!`).
    /// Use `--live` first when diagnosing "is anything happening right
    /// now / did anything just fail".
    Status {
        /// Include live runtime snapshot: in-flight jobs, recent
        /// failures (last 24h), cost-spent today.
        #[arg(long)]
        live: bool,
    },
    /// Run readiness checks against config, storage, and env. Aggregates
    /// `AuraConfig::validate`, a storage ping, an LLM probe, and an
    /// env-var audit into a single report.
    Doctor,
    /// Interactive first-run wizard: bootstrap the workspace, mint
    /// the master encryption key, register an LLM provider, optionally
    /// configure a channel bot and (in full mode) the browser tool,
    /// then launch the gateway with the dashboard open.
    ///
    /// Idempotent: safe to re-run after the workspace exists. The
    /// "Add another / Skip" picker on the LLM and channel steps lets
    /// you extend an existing setup.
    Setup,
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
    /// confirmation. Deletes the row and removes its vault secret; a
    /// running gateway's reconciler pushes a `StopBot` to the sidecar
    /// on the next tick.
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
    /// Delete an approved or pending pairing. Subsequent messages
    /// from the triple trigger a fresh pending row with a fresh
    /// code.
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
    /// List every registered LLM entry — name, provider, model, and
    /// the env var the api key is read from. Includes a `*` marker
    /// on the entry currently selected as `default-llm`.
    Status,
    /// Probe a configured LLM entry by chatting against its model and
    /// timing the round-trip.
    ///
    /// Without `name`, an interactive picker opens; pass an entry name
    /// to skip the picker.
    Probe {
        /// Optional entry name. Omit for an interactive picker.
        name: Option<String>,
    },
    /// List the live catalog reported by an LLM entry's provider —
    /// useful before running `aura llm add` to see which model ids the
    /// account has access to.
    ///
    /// Without `name`, an interactive picker opens; pass an entry name
    /// to skip the picker.
    LiveModel {
        /// Optional entry name. Omit for an interactive picker.
        name: Option<String>,
    },
    /// Interactive flow that registers a new LLM entry: pick a built-in
    /// provider, supply a name + base URL + credentials (api-key
    /// stored in the vault, OAuth flow for subscription providers),
    /// then choose the model from the live catalog. Persists the
    /// resulting entry to the active config file.
    Add,
    /// Interactive editor that lets you change individual fields of
    /// an existing LLM entry — model, base URL, API key, reasoning
    /// effort, vision override. Each field shows the current value as
    /// a placeholder; press enter to keep it.
    Edit,
    /// Interactive picker that deletes an LLM entry, clears its
    /// vault-stored api key (and OAuth bundle for subscription
    /// providers), and persists the config. Refuses to remove the
    /// entry currently named in `default-llm`.
    Remove,
    /// Interactive picker that updates `default-llm` to the chosen
    /// entry and persists the change to the active config file.
    Default,
}

#[derive(Debug, Subcommand)]
pub enum SessionCmd {
    /// List known sessions, newest-active first.
    List,
    /// Show session metadata plus, when the trace graph is wired, an
    /// execution-trace summary (jobs / steps / spans recorded under
    /// this session).
    Show {
        /// Session id.
        id: String,
    },
    /// Show the chat transcript for a session. By default, only the
    /// *active* (non-superseded) messages are returned — this is what
    /// the LLM currently sees on the next turn. Pass `--include-superseded`
    /// to also surface messages that compaction has replaced, or
    /// `--superseded-only` to filter to just those.
    ///
    /// Each row in the extended view is tagged either `[active]` or
    /// `[→ #N]` (replaced by the row at ordinal N — usually a
    /// compaction summary).
    History {
        /// Session id.
        id: String,
        /// Include rows that compaction has dropped from the active
        /// set. Surfaces both `[active]` and `[→ #N]` rows.
        #[arg(long, conflicts_with = "superseded_only")]
        include_superseded: bool,
        /// Restrict to rows that compaction has dropped from the
        /// active set.
        #[arg(long)]
        superseded_only: bool,
    },
    /// Export the full execution trace (job → step → span tree) as
    /// pretty JSON. Prints to stdout unless `--out <path>` is given,
    /// which writes the file in argv mode. Requires `--yes` under
    /// slash mode when `--out` is set, because the write path is
    /// operator-controlled.
    Export {
        /// Session id whose trace to export.
        id: String,
        /// Optional output file path. Without it, JSON is returned in
        /// the command output.
        #[arg(long)]
        out: Option<String>,
        /// Confirm the file write (required in slash mode when
        /// `--out` is set).
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum JobCmd {
    /// List tracked jobs, optionally filtered by status.
    List {
        /// Filter by status: pending, in-progress, stuck, cancelled,
        /// failed, completed.
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

/// Status filter accepted by `aura job list --status`. Mirrors
/// `aura_job::JobStatusKind` 1:1 — adding a new kind here without
/// the matching `From` arm in `commands/job.rs` is a compile error.
#[derive(Debug, Copy, Clone, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum JobStatusArg {
    Pending,
    InProgress,
    Stuck,
    Cancelled,
    Failed,
    Completed,
}

#[derive(Debug, Subcommand)]
pub enum CronCmd {
    /// List scheduled cron jobs across every user. Operator read-only view;
    /// all mutations (create/delete/enable/run) are driven through the LLM
    /// tools (`CronCreate`, `CronDelete`, …).
    List,
    /// Show a single cron job by id. Includes the prompt body and
    /// `origin_session_id` — the rest of the fields mirror what `list`
    /// surfaces per row.
    Show {
        /// Cron job id (UUID, as printed by `cron list`).
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum LogCmd {
    /// Read the main gateway / agent log
    /// (`<workspace>/logs/aura.log.<date>`).
    Main {
        /// Date of the rolling log to read, in `YYYY-MM-DD`.
        /// Defaults to today (UTC — matches the appender's daily rotation).
        #[arg(long)]
        date: Option<String>,
        /// Return only the last N lines (default: 200).
        #[arg(long, short = 'n', default_value_t = 200)]
        limit: usize,
        /// After printing the tail, stream new lines as they are
        /// appended (tail-f). Exits on Ctrl-C. Incompatible with `--json`.
        #[arg(long, short = 'f')]
        follow: bool,
    },
    /// Read a channel sidecar's log
    /// (`<workspace>/logs/channel/<channel>.log.<date>`).
    Channel {
        /// Channel type (e.g. `telegram`, `slack`, `tui`). Restricted
        /// to `[a-z0-9_-]+` so it can't escape `logs/channel/` through
        /// a path-traversal payload.
        #[arg(value_parser = crate::commands::parse_channel_name)]
        channel: String,
        /// Date of the rolling log to read, in `YYYY-MM-DD`.
        /// Defaults to today (UTC).
        #[arg(long)]
        date: Option<String>,
        /// Return only the last N lines (default: 200).
        #[arg(long, short = 'n', default_value_t = 200)]
        limit: usize,
        /// After printing the tail, stream new lines as they are
        /// appended (tail-f). Exits on Ctrl-C. Incompatible with `--json`.
        #[arg(long, short = 'f')]
        follow: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum CostCmd {
    /// Show aggregated spend. Default scope is the current UTC day
    /// across every user. Pass `--user`, `--session`, or `--job` to
    /// narrow; the scopes are mutually exclusive.
    Show {
        /// Aggregate over a single user.
        #[arg(long, short = 'u', conflicts_with_all = ["session", "job"])]
        user: Option<String>,
        /// Aggregate over a single session.
        #[arg(long, short = 's', conflicts_with_all = ["user", "job"])]
        session: Option<String>,
        /// Aggregate over a single job.
        #[arg(long, short = 'j', conflicts_with_all = ["user", "session"])]
        job: Option<String>,
        /// Lower bound (inclusive) for the time range, `YYYY-MM-DD` UTC.
        /// Defaults to start-of-today. Ignored when `--session`/`--job`
        /// are set (those scopes are bounded by the row itself).
        #[arg(long)]
        since: Option<String>,
        /// Upper bound (exclusive) for the time range, `YYYY-MM-DD` UTC.
        /// Defaults to start-of-tomorrow.
        #[arg(long)]
        until: Option<String>,
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

/// Parse argv into a `Cli`. When [`ENV_HELP_AGENT`] is set, every
/// hidden subcommand and arg is unhidden *before* clap processes
/// argv, so `aura --help` and `aura <subcmd> --help` naturally print
/// the extended view through clap's own help machinery — no separate
/// printer path to keep in sync.
///
/// Equivalent to `Cli::parse()` when the env var is unset.
pub fn parse_args() -> Cli {
    use clap::{CommandFactory, FromArgMatches};

    let cmd = Cli::command();
    let cmd = if std::env::var_os(ENV_HELP_AGENT).is_some_and(|v| !v.is_empty()) {
        unhide_recursive(cmd)
    } else {
        cmd
    };
    let matches = cmd.get_matches();
    Cli::from_arg_matches(&matches).expect("derive(Parser) matches the auto-derived Command")
}

fn unhide_recursive(cmd: clap::Command) -> clap::Command {
    let cmd = cmd.hide(false).mut_args(|a| a.hide(false));
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .collect();
    let mut cmd = cmd;
    for name in names {
        cmd = cmd.mut_subcommand(&name, unhide_recursive);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// `unhide_recursive` must clear `hide` on every nested subcommand
    /// and arg — the hide policy fans out into the `AURA_HELP_AGENT`
    /// extended-help view via the env-var-driven `parse_args` path.
    /// Walks the tree two levels deep on a representative branch to
    /// keep the guarantee honest.
    #[test]
    fn unhide_recursive_clears_hidden_on_subcommands_and_args() {
        let cmd = unhide_recursive(Cli::command());
        for sub in cmd.get_subcommands() {
            assert!(
                !sub.is_hide_set(),
                "subcommand `{}` should be unhidden",
                sub.get_name()
            );
            for arg in sub.get_arguments() {
                assert!(
                    !arg.is_hide_set(),
                    "arg `{}` on `{}` should be unhidden",
                    arg.get_id(),
                    sub.get_name()
                );
            }
        }
    }

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
