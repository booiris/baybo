# cli - Command Layer and Slash Dispatcher

## Overview

The `aura-cli` crate is the **operator-facing command layer** for Aura. It does two things and only two things:

1. **Argv mode** — `aura <command>` executes a one-shot command against the running (or freshly-loaded) domain graph and exits. Example: `aura config show`, `aura job list`, `aura session export <id>`.
2. **Slash mode** — while a user is chatting over any channel, lines starting with `/` (e.g. `/config show`, `/cron list`) are intercepted by the channel adapter, dispatched through the same parser and handlers as Argv mode, and their output returned to the user as a normal response. Slash commands **do not** enter the agent's conversation context.

`aura-cli` adds no business logic. Every command is a thin adapter that turns parsed flags into an existing manager call (`SessionManager`, `JobLifecycle`, `ToolRegistry`, `SkillRegistry`, `CronScheduler`, `SecretVault`, `MemoryManager`, `WorkspaceManager`, `AuraConfig`). When a subsystem is not yet implemented, its command family is omitted — the CLI never surfaces a "zombie" command that prints `not implemented`.

The command taxonomy is organized by subsystem: one family per manager exposed in `src/main.rs`. Subsystems that do not yet exist in Aura (e.g. service-mode gateway, device pairing, browser control) get no command family at all.

## Design Decisions

### Single parser, two entry points

`Argv` and `Slash` share one `clap::Command` tree built by the `#[derive(Parser)]` types in `cli.rs`. Slash input is shell-tokenized (`shell-words`), the leading `/` is stripped, and the tokens are fed to `Cli::try_parse_from`. The resulting `Commands` enum goes through the same `dispatch::run(ctx, cmd)` async function. This guarantees that `aura foo --bar baz` and `/foo --bar baz` always produce identical results — there is no second parser to drift.

### Slash commands do not touch agent context

A slash command never becomes a `ChatMessage`, is never seen by the LLM, and is never appended to the session history. It is a side-channel into the domain graph. This is a hard invariant: dispatching `/config set …` in the middle of a conversation must not pollute the model's context with operator chatter.

Reserved slash tokens (`/quit`, `/exit`, `/clear`) stay local to the adapter and never reach the dispatcher — they control the terminal, not the domain graph.

### `CommandContext` is the only handle

Every command receives a `CommandContext` carrying `Arc` clones of the managers plus the loaded `AuraConfig` and an `OutputSink`. The same struct is used in both modes; only the sink differs (`StdoutSink` vs `ChannelResponseSink`). Commands never reach for globals, never construct their own `Arc`s, and never take `&mut` to any manager — concurrency safety is the manager's responsibility.

### Explicit mutation confirmation in slash mode

A command is marked `Mutating` at definition time (read-only commands default to `ReadOnly`). Mutating commands invoked over slash require an explicit `--yes` (or `-y`) flag; without it the dispatcher returns `CliError::ConfirmationRequired` and the response explains what would have happened. Argv mode allows interactive prompts; slash mode does not (there is no TTY guarantee on non-CLI channels). This keeps mis-typed `/job cancel foo` from firing silently while a user is chatting.

### Output format is structured, not just printed

Commands return `CommandOutput`, not `String`. The sink decides how to render:

- `OutputFormat::Human` → pretty text / ANSI tables / sectioned summaries
- `OutputFormat::Json` → `serde_json::to_string_pretty` for scripts
- `OutputFormat::Plain` → uncolored single-block text, used by the slash sink when sending back over a channel

`--json` and `--plain` are global flags and work identically in both modes — `/job list --json` returns a JSON-formatted text block.

### Shell completion is built-in

`aura completion <shell>` uses `clap_complete` to emit bash/zsh/fish/powershell/elvish scripts. No extra scaffolding is needed because clap already owns the tree.

### No new error enum

`CliError` (thiserror) carries CLI-specific variants (`Parse`, `ConfirmationRequired`, `UnknownCommand`, `AgentSendForbiddenInSlash`, `NotAvailableInSlash`) plus catch-all wrapper variants (`Config`, `Io`, `Serialization`, `Manager`). `From` impls cover `std::io::Error`, `serde_json::Error`, `aura_config::ConfigError`, and `aura_setup::SetupError`; every other domain error is wrapped via `.to_string()` so `aura-cli` does not take a hard dependency on every domain crate's error enum.

### `SlashHandler` lives in `channels`

The trait that lets a channel adapter intercept `/` input is defined in `aura-channels` (not `aura-cli`). `aura-cli` _implements_ the trait but does not own it. This matters for dependency direction: the gateway WS transport and any future telegram/discord sidecar can accept an `Arc<dyn SlashHandler>` without any of them depending on `aura-cli`.

## Command Reference

**Global flags** (apply to every command in both modes):
`--config <path>` · `--json` · `--plain` · `--no-color`

(`-V`/`--version` is clap's root `aura --version`, derived on the top-level `Cli`, not a `GlobalArgs` member.)

`--config` is UX sugar: `main` writes its value into `AURA_CONFIG_PATH`
once at startup, and every downstream reader goes through the env var.
So both `aura --config /foo/aura.json …` and
`AURA_CONFIG_PATH=/foo/aura.json aura …` hit the same code path, and
installed services (systemd/launchd) that only have env available work
without special cases.

### `AURA_HELP_AGENT` (extended help)

When `AURA_HELP_AGENT` is set to any non-empty value, `aura --help` (and
every `aura <subcmd> --help`) surfaces the flags and subcommands hidden
from the default view. The goal is to keep the headline surface focused
on what most operators reach for and keep agent / log / trace inspection
one env var away for the moments it matters.

The mechanism lives in `aura_cli::cli::parse_args`: it checks the env
var, swaps in an unhidden clap `Command` *before* parsing, and then
clap's own `--help` machinery prints the extended view. There is no
custom help printer to keep in sync.

Hidden by default — only listed when `AURA_HELP_AGENT` is set:

- Subcommands: `config`, `session`, `job`, `cron`, `log`, `cost`

`session` is the unified "everything about a session" surface: metadata
(`show`), chat transcript (`history`), and full execution-trace JSON
(`export`). The earlier separate `trace` family was folded back into
`session` — operators kept hitting the "which command shows me what
happened in session X" branch and the split bought nothing. The
execution-trace summary (jobs / steps / spans counts) is now appended
to `session show` directly when the trace graph is wired.

`log` is a distinct family because it reads the **rolling tracing
files on disk** (`logs/aura.log.<date>`, `logs/channel/<ch>.log.<date>`),
not the structured `TraceStore`. Different store, different read
shape — kept top-level.

The hide policy lives next to the clap tree in
`crates/cli/src/cli.rs`: each hidden surface carries `hide = true` (args)
or `#[command(hide = true)]` (subcommands), and `unhide_recursive`
walks the `Command` flipping `hide(false)` when the env var is set.
Add a new debug-only surface by setting `hide = true` on its
arg/variant — `unhide_recursive` picks it up automatically.

**Agent-side opt-in**: `aura-tools::builtin::bash::inject_aura_env`
prefixes any tool command containing the literal token `aura` with
`export AURA_HELP_AGENT=1; export AURA_CONFIG_PATH=<absolute path>;`
before the subshell runs. The agent gets:

* the full help inventory out of the box (`AURA_HELP_AGENT`);
* the same config the parent process is reading — reads
  `AURA_CONFIG_PATH` from the parent env, falling back to
  `aura_workspace::paths::default_config_file` so the child `aura`
  never silently looks at a different workspace. The path is
  resolved to absolute via `std::path::absolute` so a relative
  debug-mode default still points at the right place after the
  bash tool changes cwd.

The substring match is loose; non-aura processes inherit the
variables and ignore them, so a false-positive injection is a no-op.

"Status" shows what actually ships today. Rows marked **deferred** are kept here so future contributors can see the target surface; the missing backing APIs land with their subsystems — the original mass-tracker was completed and archived at `docs/todo/archives/cli-write-commands.md`. Handlers for deferred subcommands do not exist — the clap tree in `crates/cli/src/cli.rs` only exposes the shipped rows.

| Family       | Subcommands                                                                                               | Backing module                                                               | Mutation                                                                                           | Status                                            |
| ------------ | --------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| `config`     | `show [section]` · `file` · `schema` · `validate`                                                         | `AuraConfig` + `boot::load_config`                                           | read-only                                                                                          | shipped                                           |
| `config`     | `get <path>` · `set <path> <value>` · `unset <path>`                                                      | `AuraConfig::{set_at_path, unset_at_path, write_to_file}`                    | `set`/`unset` write `aura.json`; take effect after restart (hot-reload deferred — see `config.md`) | shipped                                           |
| `skills`     | `list` · `info <name>`                                                                                    | `SkillRegistry`                                                              | read-only                                                                                          | shipped                                           |
| `skills`     | `search [query]` · `check [name]`                                                                         | `SkillRegistry::search` / `validate_all`                                     | read-only; `check` validates declared `required_bins` on `$PATH`, `required_env` in env, and basic declarative shape; `required_models` is reported as a note only | shipped                                           |
| `channel`    | `list` · `add` · `remove`                                                                                 | `ChannelBotStore` + vault                                                    | `add`/`remove` mutate; `list` is read-only                                                         | shipped                                           |
| `channel`    | `status` · `logs [channel]`                                                                               | `ChannelRegistry` / adapter logs                                             | read-only                                                                                          | deferred — needs per-adapter status + log drain   |
| `mcp`        | `add <name> <command-or-url> [...]` · `list` · `get <name>` · `remove <name>`                              | `aura-tools::mcp` (config: `<workspace>/.mcp.json`) + `SecretVault` (tokens) | `add`/`remove` mutate `.mcp.json` + vault; `add` runs the OAuth flow inline when an OAuth flag is passed for an HTTP server. The running gateway's `McpReconciler` picks up changes within ~5s. | shipped                                           |
| `llm`        | `status`                                                                                                  | `LlmClient`                                                                  | read-only                                                                                          | shipped                                           |
| `llm`        | `probe [name]` · `live-model [name]`                                                                      | `LlmProviderRegistry::list_models` / `LlmClient::probe`                      | `probe` issues a minimal chat request                                                              | shipped                                           |
| `llm`        | `add` · `edit` · `remove` · `default`                                                                     | Interactive editors that write the active config + vault                     | mutates `aura.json` and per-entry vault keys                                                       | shipped                                           |
| `external-agent` | `status` · `setup` · `disable` · `default`                                                            | `aura-agent::external_agent` CLI backends (Claude Code / Codex / Gemini)     | `status` re-probes each kind offline (read-only); `setup` is an interactive wizard that probes a binary path and writes the resolved **absolute** `external_agents.<kind>.binary_path` to `aura.json` (empty input = PATH lookup, still recorded as a concrete path); `disable` is a multi-select that flips `external_agents.<kind>.enabled = false` for the checked kinds and re-resolves the default (no-op success when nothing is enabled); `default` sets `external_agents.default_external_agent` to an enabled kind (operator-facing designation — nothing reads it at runtime yet, so no restart) | shipped                                           |
| `session`    | `list` · `show <id>` · `history <id> [--include-superseded \| --superseded-only]` · `export <id> [--out <path>]` | `SessionManager` + `QueryApi::replay`                                        | read-only. `show` returns metadata + message count + (when `QueryApi` is wired) jobs/steps/spans counts from the trace store. `history` defaults to the *active* (non-superseded) transcript; `--include-superseded` walks the full log and tags each row `[active]` or `[→ #N]`, `--superseded-only` keeps just the dropped rows. `export` writes the full fork-aware call tree as pretty JSON (stdout, or `--out <path>` with `--yes` required in slash mode). | shipped                                           |
| `job`        | `list [--status]` · `show <id>` · `cancel <id>`                                                           | `JobLifecycle`                                                               | `cancel` mutates                                                                                   | shipped                                           |
| `cron`       | `list` · `show <id>`                                                                                      | `CronScheduler::list_all_jobs` / `get_job`                                   | read-only operator view. `show` returns the full job row (prompt body + `origin_session_id` + timestamps); cron jobs are bound to `user_id + channel`, not to a session, so a session's audit trail of cron creations is better viewed via `session export` than a cron-side filter. All cron mutations (create/delete/enable/disable/run) are driven through the LLM tools (`CronCreate`, `CronDelete`, `CronList`) registered by `aura-cron::tools::agent_tools`. | shipped                                           |
| `log`        | `main [--date <YYYY-MM-DD>] [-n <limit>] [-f/--follow]` · `channel <channel> [--date] [-n] [-f]` | Workspace `logs/` files (`logs/aura.log.<date>`, `logs/channel/<ch>.log.<date>`) written by the tracing appender | read-only. Tails the last `--limit` lines (default 200) by seeking backwards from EOF; `--follow` polls for appended bytes until Ctrl-C (incompatible with `--json`). | shipped                                           |
| `security`   | `audit` · `leaks check <file>`                                                                            | `SecurityGateway::audit` / `LeakDetector::check_file`                        | read-only; `audit` would return rule count by action + vault master-key flag (never secret material); `leaks check` would report blocked/hits via the shared detector | deferred — no `Security` variant in the clap tree yet |
| `cost`       | `show [--user <u> \| --session <id> \| --job <id>] [--since <YYYY-MM-DD>] [--until <YYYY-MM-DD>]`        | `QueryApi::cost_summary` (`CostScope::{User, Session, Job, TimeRange}`)      | read-only. Scopes are mutually exclusive: `--user` is bounded by `--since`/`--until` (default = current UTC day); `--session`/`--job` ignore the time range. Output reports total micro-USD + token aggregates (input / output / cached input / cache writes). | shipped (requires the full domain graph; returns a `Manager` error in argv-light boots that lack `QueryApi`) |
| `status`     | `[--live]`                                                                                                | Static: registries + `LlmClient`. Live: `JobLifecycle::list` + `QueryApi::cost_summary` | `--live` adds in-flight job count, failed-jobs-last-24h, and today's spend (USD + token counts). Each live counter degrades to `(unavailable)` when its manager isn't wired in the current invocation. | shipped (live block populated where managers are wired)  |
| `gateway`    | `start` · `install [--system] [--exec-start <p>]` · `enable` · `disable` · `uninstall` · `status` · `token {show, rotate}` | `aura-gateway` installer + `AdminToken`                                      | `start` runs the long-lived server; `install`/`enable`/`disable`/`uninstall` and `token rotate` mutate; `status`/`token show` are read-only | shipped (intercepted in `src/main.rs` before dispatch, runs in `src/gateway_cmd.rs`) |
| `pair`       | `list [--pending\|--approved]` · `approve <code>` · `revoke <channel-type> <bot-id> <user-id>`            | `aura-pair` store via `ChannelPairingStore`                                  | `approve`/`revoke` mutate                                                                          | shipped                                           |
| `prompt`     | `[PROMPT] [--session <id>] [-y/--dangerously-allow-all] [--timeout <secs>]`                               | Hybrid: WS into a live gateway, else in-process `runtime::build_managers` + `wire_router` | runs one agent turn — persists the session row + transcript + traces + cost like any conversation | shipped (intercepted before dispatch, runs in `src/prompt_cmd.rs`) |
| `tui`        | `[--session <id>]`                                                                                        | WS client into the gateway's channel listener                                | read-only                                                                                          | shipped (intercepted before dispatch, runs in `src/tui_cmd.rs`) |
| `setup`      | —                                                                                                         | Interactive first-run wizard (`aura-setup`)                                  | bootstraps workspace + master key + default `aura.json`                                            | shipped (intercepted before dispatch, runs in `src/setup_cmd.rs`) |
| `doctor`     | —                                                                                                         | Aggregates `AuraConfig::validate`, storage ping, `llm::probe`, env-var audit | read-only                                                                                          | shipped (LLM probe gated on `llm probe` landing)  |
| `completion` | `<shell>`                                                                                                 | `clap_complete`                                                              | stdout only                                                                                        | shipped                                           |

### `prompt` — headless one-shot turns

`aura prompt [PROMPT]` runs a single agent turn non-interactively: stream
the assistant's answer to stdout, then exit. It is the non-interactive
sibling of `tui` — same agent, no UI. With no `PROMPT` argument the text
is read from stdin (`git diff | aura prompt "review this"`, `cat task.md |
aura prompt`); an argument *plus* piped stdin are concatenated. Lives in
`src/prompt_cmd.rs`, intercepted in `main.rs` before the generic argv
dispatch (same as `tui`).

**Hybrid runtime, keyed off the singleton lock.** A running gateway holds
the `<workspace>/state/aura.lock` flock for its lifetime (`src/singleton.rs`),
so `prompt` uses lock acquisition as a gateway-presence probe:

- **Lock held** (a gateway is up) → connect over the same `/v1/channel-ws`
  + `WsTransport` path the TUI uses and run the turn through the existing
  gateway. One owner of the session state, no contention.
- **Lock free** (no gateway) → acquire it and build the agent runtime
  in-process for the single turn via `runtime::build_managers` +
  `wire_router`, then tear it down. This is what lets `aura prompt` work
  standalone with no separate `aura gateway` process. The lock is held for
  the whole turn, so a concurrent `prompt` or a gateway start can't race
  the same workspace (the "session data has a single owner" invariant).
  Output is collected by attaching an in-process `ConnectionSink` to the
  `tui` channel and subscribing it to the session — the in-process mirror
  of the gateway's WS sink.

**Output.** stdout carries only the assistant's answer (streamed as it
generates); reasoning / tool / status events are dropped and notices go to
stderr, so `aura prompt … > out.txt` captures just the answer. Tracing
goes to stderr too (`TracingMode::Stderr`, default `warn`). `--json`
instead buffers and emits one object on stdout: `{"session_id",
"response"}` on success, or `{"session_id", "error"}` (with a non-zero
exit and nothing on stderr) on failure — so a JSON consumer always gets a
parseable result *and* the session id, whichever way the turn goes.

**Session id & resume.** The *client* pins the session id — either an
explicit `--session <id>` (pick a memorable one up front) or a fresh UUID
minted per run. A run started without `--session` exposes its id **only**
via `--json` (`sid=$(aura prompt "…" --json | jq -r .session_id)`);
plain-text output is just the answer, so capture the id with `--json` (or
choose your own up front with `--session`) when you intend to resume.

Resume with `aura prompt --session <id> "next turn"`: the agent rehydrates
that session's context (server-side under a gateway, from the durable row
in-process) and only the new turn's output is printed — the prior
transcript is not replayed (the client subscribes with `since_ordinal:
None`). This mirrors Claude Code's `claude -p --output-format json` →
`--resume <id>`, except Aura's client (not the server) assigns the id.

**Tool approvals** have no human to answer them. Default is `Deny`
(fail-closed — the turn continues and the model adapts to the denial);
`-y`/`--dangerously-allow-all` switches to `Approve`. The WS path
auto-resolves the gateway's approval prompt over the wire; the in-process
path installs a session-scoped auto gate that resolves instantly without
fanning a prompt out to a UI that isn't there.

**Persistence.** A `prompt` turn persists exactly like any conversation:
the session row (created lazily on the first message), the user +
assistant transcript, trace steps/spans, and cost records. Per the
"session data is core data — never delete" rule, each `prompt` without
`--session` leaves a permanent session row. Because the in-process path
exits seconds after the answer, it awaits `CostManager::drain` before
teardown to flush the fire-and-forget `cost_records` write that the
long-running gateway would otherwise outlive.

**Bounding the wait.** A rejected `handle_incoming` — rate-limit, budget
(`CostManager::check`), or security — returns an error the router only
*logs* (`router/mod.rs`); nothing is dispatched to the client, so the
consume loop would otherwise block forever waiting for a reply that never
comes. `--timeout <secs>` (default 300; `0` = wait indefinitely) caps the
wait for the turn's reply: on expiry the turn errors out (surfaced as the
`error` field under `--json`). Raise it for genuinely long agentic turns.

### Deferred command families

Listed so future contributors see the gap explicitly. Each one waits for its subsystem to land:

- **Service lifecycle**: `daemon` — service installation lives under `aura gateway`; no separate `daemon` surface.
- **Device and node fabric**: `nodes`, `devices` — no paired-peer concept in Aura today. (Per-user pairing approval ships as `aura pair`.)
- **IDE / external bridges**: `acp`, `mcp serve`, `dashboard` — out of scope until the corresponding subsystems exist. The MCP **client** ships as `aura mcp {add,list,get,remove}` (see the row above); only the *server-side* `mcp serve` family remains deferred.
- **Rich media**: `browser`, inference over image/audio/video/tts — no Aura counterpart.
- **Plugin distribution & installer**: `plugins`, `backup`, `update`, `onboard`, `reset` — release-engineering concerns, not runtime.
- **Auxiliary directories**: `directory`, `wiki`, `webhooks`, `dns` — deferred until each subsystem lands.

Each family is added under the same naming scheme when its subsystem ships.

## Slash Integration

### Wiring

`aura tui` is a `/v1/channel-ws` client into a running gateway: `src/tui_cmd.rs` connects via `WsTransport`, builds `aura_tui::client::TuiSlashHandler` and `TuiDashboardProvider` (both forward calls over the WS to the gateway), and hands them to `TuiAdapter::new().with_slash_handler(...).with_dashboard_provider(...)`. The CLI's `CliSlashHandler` / `CliDashboardProvider` are defined in `aura-cli` for use by future in-process adapters but are not wired by the binary today. Other adapters (HTTP/Telegram/Discord) accept the same `Arc<dyn SlashHandler>` when they land; `DashboardProvider` is only consumed by adapters that can render interactive views (i.e. TUI).

Persistent TUI input history is owned by the gateway, not `aura-cli`. The TUI loads and appends the ring over the channel WS via `Frame::HistorySnapshot` and `Frame::HistoryAppend`; see [`tui.md`](./tui.md) and [`security.md`](./security.md). This keeps the TUI decoupled from `aura-security` and ensures a single writer (the gateway) owns the encrypted blob, so concurrent `aura tui` clients can't clobber each other.

### Dashboard shortcut

Bare dashboard commands — `/skills`, `/tools`, `/jobs`, `/sessions`, `/memory` with no further tokens — bypass the clap path and return `SlashOutcome::OpenView(ViewKind::_)`. Interactive adapters swap into a table view backed by `DashboardProvider::snapshot(kind)`; line-mode adapters treat the outcome as a no-op. Commands with arguments (e.g. `/skills info foo`) still go through clap as `SlashOutcome::Handled`.

### Skill shortcut

`CliSlashHandler::commands()` also surfaces every user-invocable skill (`SkillDefinition::command.is_some()`) so the TUI completion popup lists `/<skill>` alongside built-in commands. Clap subcommands win on name collisions. When `handle` sees `/<token>` whose first token matches a registered user-invocable skill, it returns `SlashOutcome::PassThrough` so the TUI forwards the raw line to the agent as a normal user message — `SkillRegistry::select` then narrows on the exact-match branch. This is the one sanctioned exception to the "slash ≠ chat message" invariant below: skill invocations are explicit user intent to run a skill, not operator chatter, so they belong in the conversation.

### Output rendering over channels

`ChannelResponseSink` turns `CommandOutput` into a single `ContentBlock::Text`. Tables are rendered as monospace text; `--json` produces JSON text. Images and files are never produced by CLI commands.

### Mutation feedback

When a command with `Mutating = true` runs in slash mode, its response always includes the resulting `JobId` / new row id / trace span id, so the user sees an auditable handle. This aligns with the observability constraint in CLAUDE.md.

## Constraints

- `aura-cli` holds no mutable state; all managers are `Arc`. The crate is `Send + Sync + 'static`.
- Slash input **must not** be forwarded to the agent when it parses as a known CLI command. Unknown `/` input (parse error `UnknownCommand`) falls back to `PassThrough` only if the dispatcher explicitly says so — never by default. The skill shortcut is the one sanctioned `PassThrough` path: a `/<token>` whose first token matches a user-invocable skill is forwarded to the agent as a normal chat message.
- Commands that mutate must route their effect through the manager (never touching a store directly), so traces fire naturally.
- The `SecretVault` value of any secret is never rendered; `security` and `config` commands redact to `********`.
- No `unwrap` / `expect` in command handlers. Parser-level `expect` on derive macros is acceptable.
- Every **shipped** command family has at least one parser test and one dispatch test. Deferred families do not appear in the clap tree at all until their subsystem lands, so there is nothing to test yet.

## Collaboration

| Module                                                                                                              | Role                                                                                                                                                             |
| ------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bootstrap` — the `aura` binary at the **workspace root** (`src/main.rs`, `src/boot.rs`, `src/runtime.rs`, `src/gateway_cmd.rs`, `src/tui_cmd.rs`, `src/setup_cmd.rs`), not `crates/cli` (`aura-cli` is lib-only, no `main.rs`) | Promotes `--config` into `AURA_CONFIG_PATH`, then routes to a per-subcommand entry: `gateway_cmd::run` for `gateway`, `setup_cmd::run` for `setup`, `tui_cmd::run` for `tui`, and the lightweight argv path (`aura_cli::dispatch::run` against a `CommandContext`) for everything else. The TUI side (`tui_cmd`) wires `aura-tui`'s WS-backed `TuiSlashHandler` / `TuiDashboardProvider`, not the in-crate `CliSlashHandler`. |
| `config`                                                                                                            | `config` family directly reads/writes `AuraConfig`; `doctor` calls `validate`.                                                                                   |
| `agent`                                                                                                             | Supplies all manager `Arc`s.                                                                                              |
| `channels`                                                                                                          | Owns `SlashHandler`, `SlashOutcome`, `DashboardProvider`, `ViewKind`; `TuiAdapter` is the first consumer of all four.                                            |
| `job` / `cron` / `skills` / `tools` / `session` / `security` / `llm` | Each exposes the read/write APIs that a command family calls. CLI contains no business logic — it is a parameter adapter only.                                   |

## Verification

**Phase 1 (document)** — complete.

1. `docs/modules/cli.md` exists with the seven sections above.
2. `docs/modules/README.md` lists `cli` in its module groups and Reading Order.
3. Every command family in the table maps to a manager already present in `src/main.rs`; the remaining "deferred" rows are added as their subsystems land.

**Phase 2a — read-only commands** — complete.

- `cargo fmt && cargo clippy --all --benches --tests --examples --all-features` — zero warnings.
- `cargo test -p aura-cli` — 14 parser tests + 11 dispatch smoke tests pass.
- `aura --help` / `aura <family> --help` render; slash `/config file`, `/skills list`, `/channel list` return the same payloads as their argv twins.
- `aura completion zsh > /tmp/_aura && zsh -c 'source /tmp/_aura'` loads without error.
- `aura doctor` reports an error when `security.encryption_key_file` is missing or unreadable, and when no LLM client is configured.

**Phase 2b — write-mutating commands** — complete. Tracked in `docs/todo/archives/cli-write-commands.md` (archived). Each shipped family landed with the following:

- Parser snapshot test in `crates/cli/tests/parser.rs` (aggregated via `crates/cli/tests/all.rs`).
- Dispatch smoke tests in `crates/cli/tests/mcp_e2e.rs` (also reached via `all.rs`).
- Slash-mode confirmation test (missing `--yes` returns `CliError::ConfirmationRequired`).