# cli - Command Layer and Slash Dispatcher

## Overview

The `aura-cli` crate is the **operator-facing command layer** for Aura. It does two things and only two things:

1. **Argv mode** — `aura <command>` executes a one-shot command against the running (or freshly-loaded) domain graph and exits. Example: `aura config show`, `aura job list`, `aura trace export <id>`.
2. **Slash mode** — while a user is chatting over any channel, lines starting with `/` (e.g. `/config show`, `/cron list`) are intercepted by the channel adapter, dispatched through the same parser and handlers as Argv mode, and their output returned to the user as a normal response. Slash commands **do not** enter the agent's conversation context.

`aura-cli` adds no business logic. Every command is a thin adapter that turns parsed flags into an existing manager call (`SessionManager`, `JobManager`, `TraceCollector`, `ToolRegistry`, `SkillRegistry`, `CronScheduler`, `SecretVault`, `MemoryManager`, `WorkspaceManager`, `AuraConfig`). When a subsystem is not yet implemented, its command family is omitted — the CLI never surfaces a "zombie" command that prints `not implemented`.

The command taxonomy is organized by subsystem: one family per manager exposed in `src/main.rs`. Subsystems that do not yet exist in Aura (e.g. service-mode gateway, device pairing, browser control) get no command family at all.

## Design Decisions

### Single parser, two entry points

`Argv` and `Slash` share one `clap::Command` tree built by the `#[derive(Parser)]` types in `cli.rs`. Slash input is shell-tokenized (`shell-words`), the leading `/` is stripped, and the tokens are fed to `Cli::try_parse_from`. The resulting `Commands` enum goes through the same `dispatch::run(ctx, cmd)` async function. This guarantees that `aura foo --bar baz` and `/foo --bar baz` always produce identical results — there is no second parser to drift.

### Slash commands do not touch agent context

A slash command never becomes a `ChatMessage`, is never seen by the LLM, and is never appended to the session history. It is a side-channel into the domain graph. This is a hard invariant: dispatching `/memory clear …` in the middle of a conversation must not pollute the model's memory with operator chatter.

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

`aura completion <shell>` uses `clap_complete` to emit bash/zsh/fish/powershell scripts. No extra scaffolding is needed because clap already owns the tree.

### No new error enum

`CliError` (thiserror) wraps `clap::Error`, `ConfigError`, `AgentError`, `TraceError`, etc. via `#[from]`. Per CLAUDE.md, each crate owns its error enum; `aura-cli` adds only the CLI-specific variants (`ParseError`, `ConfirmationRequired`, `UnknownCommand`, `AgentSendForbiddenInSlash`).

### `SlashHandler` lives in `channels`

The trait that lets a channel adapter intercept `/` input is defined in `aura-channels` (not `aura-cli`). `aura-cli` _implements_ the trait but does not own it. This matters for dependency direction: the gateway WS transport and any future telegram/discord sidecar can accept an `Arc<dyn SlashHandler>` without any of them depending on `aura-cli`.

## Command Reference

**Global flags** (apply to every command in both modes):
`--config <path>` · `--profile <name>` · `--json` · `--plain` · `--no-color` · `-v/--verbose` · `-V/--version`

`--config` is UX sugar: `main` writes its value into `AURA_CONFIG_PATH`
once at startup, and every downstream reader goes through the env var.
So both `aura --config /foo/aura.json …` and
`AURA_CONFIG_PATH=/foo/aura.json aura …` hit the same code path, and
installed services (systemd/launchd) that only have env available work
without special cases.

"Status" shows what actually ships today. Rows marked **deferred** are kept here so future contributors can see the target surface; the missing backing APIs are tracked in the per-subsystem follow-up todos (`docs/todo/cli-agent-send-argv.md`) — the original mass-tracker was completed and archived at `docs/todo/archives/cli-write-commands.md`. Handlers for deferred subcommands do not exist — the clap tree in `crates/cli/src/cli.rs` only exposes the shipped rows.

| Family       | Subcommands                                                                                               | Backing module                                                               | Mutation                                                                                           | Status                                            |
| ------------ | --------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| `config`     | `show [section]` · `file` · `schema` · `validate`                                                         | `AuraConfig` + `boot::load_config`                                           | read-only                                                                                          | shipped                                           |
| `config`     | `get <path>` · `set <path> <value>` · `unset <path>`                                                      | `AuraConfig::{set_at_path, unset_at_path, write_to_file}`                    | `set`/`unset` write `aura.json`; take effect after restart (hot-reload deferred — see `config.md`) | shipped                                           |
| `skills`     | `list` · `info <name>`                                                                                    | `SkillRegistry`                                                              | read-only                                                                                          | shipped                                           |
| `skills`     | `search [query]` · `check [name]`                                                                         | `SkillRegistry::search` / `validate_all`                                     | read-only; `check` validates declared `required_bins` on `$PATH`, `required_env` in env, and basic declarative shape; `required_models` is reported as a note only | shipped                                           |
| `tools`      | `list` · `info <name>`                                                                                    | `ToolRegistry`                                                               | read-only                                                                                          | shipped                                           |
| `channel`    | `list`                                                                                                    | `ChannelRegistry`                                                            | read-only                                                                                          | shipped                                           |
| `channel`    | `status` · `logs [channel]`                                                                               | `ChannelRegistry` / adapter logs                                             | read-only                                                                                          | deferred — needs per-adapter status + log drain   |
| `llm`        | `status`                                                                                                  | `LlmClient`                                                                  | read-only                                                                                          | shipped                                           |
| `llm`        | `models` · `probe`                                                                                        | `LlmProviderRegistry::list_models` / `LlmClient::probe`                      | `probe` issues a minimal chat request                                                              | shipped                                           |
| `workspace`  | `show`                                                                                                    | `WorkspaceManager`                                                           | read-only                                                                                          | shipped                                           |
| `workspace`  | `set-identity <name> (--file <path> \| --content <text>)`                                                 | `WorkspaceManager::write_identity_file`                                      | overwrites the target `*.md` atomically; running process keeps the old content until restart (requires `--yes` in slash mode) | shipped                                           |
| `session`    | `list` · `show <id>` · `history <id>` · `kill <id>`                                                       | `SessionManager`                                                             | `kill` mutates                                                                                     | shipped                                           |
| `job`        | `list [--status]` · `show <id>` · `cancel <id>`                                                           | `JobManager`                                                                 | `cancel` mutates                                                                                   | shipped                                           |
| `trace`      | `list [--session <id>] [--limit <n>]` · `show <id>` · `export <id> [--out <path>]`                        | `TraceStore::query_traces` / `load_trace`                                    | `export --out` writes a local file (requires `--yes` in slash mode)                                | shipped                                           |
| `trace`      | `snapshot <session-id> [--node <id>] [--full]`                                                            | `TraceStore::load_trace` + `aura_trace::snapshot::find_nearest_snapshot`     | read-only ancestor lookup on the stored trace; does **not** capture a new live snapshot (that still needs session context the CLI does not hold) | shipped (stored lookup); live capture remains deferred |
| `cron`       | `list`                                                                                                    | `CronScheduler::list_all_jobs`                                               | read-only operator view. All cron mutations (create/delete/enable/disable/run) are driven through the LLM tools (`CronCreate`, `CronDelete`, `CronList`) registered by `aura-cron::agent_tools`. | shipped                                           |
| `memory`     | `list [--user <u>]` · `search <query> [--user <u>]` · `show <id>` · `promote <id> [--to <f>]` · `clear --session <id>` | `MemoryManager`                                                              | `promote`/`clear` mutate (require `--yes` in slash mode)                                           | shipped                                           |
| `security`   | `audit` · `leaks check <file>`                                                                            | `SecurityGateway::audit` / `LeakDetector::check_file`                        | read-only; `audit` would return rule count by action + vault master-key flag (never secret material); `leaks check` would report blocked/hits via the shared detector | deferred — no `Security` variant in the clap tree yet |
| `agent`      | `send --session <id> --message <text>`                                                                    | `Router` / `AgentLoop`                                                       | mutates; **disabled in slash mode** (returns `AgentSendForbiddenInSlash`)                          | partial — grammar + slash guard shipped; argv returns a deferred `Manager` error pending a `Router` one-shot entry |
| `status`     | —                                                                                                         | `SessionManager` + `JobManager` + `CostTracker`                              | read-only                                                                                          | shipped                                           |
| `doctor`     | —                                                                                                         | Aggregates `AuraConfig::validate`, storage ping, `llm::probe`, env-var audit | read-only                                                                                          | shipped (LLM probe gated on `llm probe` landing)  |
| `completion` | `<shell>`                                                                                                 | `clap_complete`                                                              | stdout only                                                                                        | shipped                                           |

### Deferred command families

Listed so future contributors see the gap explicitly. Each one waits for its subsystem to land:

- **Service lifecycle**: `gateway`, `daemon` — Aura currently runs as a single foreground process; no install/start/stop/restart surface yet.
- **Device and node fabric**: `nodes`, `devices`, `pairing` — no paired-peer concept in Aura today.
- **IDE / external bridges**: `acp`, `mcp serve`, `dashboard` — out of scope until the corresponding subsystems exist. (`tui` has landed as the built-in `TuiAdapter`; see [`channels.md`](./channels.md) and [`tui.md`](./tui.md).)
- **Rich media**: `browser`, inference over image/audio/video/tts — no Aura counterpart.
- **Plugin distribution & installer**: `plugins`, `backup`, `update`, `uninstall`, `setup`, `onboard`, `reset` — release-engineering concerns, not runtime.
- **Auxiliary directories**: `directory`, `wiki`, `webhooks`, `dns`, `hooks install/update` — deferred until each subsystem lands.

Each family is added under the same naming scheme when its subsystem ships.

## Slash Integration

### Wiring

`src/main.rs`, after `CommandContext` is assembled, constructs `aura_cli::CliSlashHandler::new(ctx)` and `aura_cli::CliDashboardProvider::new(ctx)`, then passes both to `TuiAdapter::new().with_slash_handler(...).with_dashboard_provider(...)`. Other adapters (HTTP/Telegram/Discord) accept the same `Arc<dyn SlashHandler>` when they land; `DashboardProvider` is only consumed by adapters that can render interactive views (i.e. TUI).

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
- Commands that mutate must route their effect through the manager (never touching a store directly), so traces and hooks fire naturally.
- The `SecretVault` value of any secret is never rendered; `security` and `config` commands redact to `********`.
- No `unwrap` / `expect` in command handlers. Parser-level `expect` on derive macros is acceptable.
- Every **shipped** command family has at least one parser test and one dispatch test. Deferred families do not appear in the clap tree at all until their subsystem lands, so there is nothing to test yet.
- `agent send` is rejected in slash mode with a clear error — firing a new agent turn from within an agent turn would either deadlock or corrupt the ongoing conversation.

## Collaboration

| Module                                                                                                              | Role                                                                                                                                                             |
| ------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `bootstrap` (`src/main.rs`, `src/boot.rs`)                                                                          | Checks argv first: if a subcommand is present, runs `aura_cli::run` (re-exported from `dispatch::run`) and exits; otherwise boots the full router and injects `CliSlashHandler` into adapters. |
| `config`                                                                                                            | `config` family directly reads/writes `AuraConfig`; `doctor` calls `validate`.                                                                                   |
| `agent`                                                                                                             | Supplies all manager `Arc`s; `agent send` reuses the `Router` path.                                                                                              |
| `channels`                                                                                                          | Owns `SlashHandler`, `SlashOutcome`, `DashboardProvider`, `ViewKind`; `TuiAdapter` is the first consumer of all four.                                            |
| `trace` / `job` / `cron` / `skills` / `tools` / `session` / `memory` / `security` / `workspace` / `llm` | Each exposes the read/write APIs that a command family calls. CLI contains no business logic — it is a parameter adapter only.                                   |

## Verification

**Phase 1 (document)** — complete.

1. `docs/modules/cli.md` exists with the seven sections above.
2. `docs/modules/README.md` lists `cli` in its module groups and Reading Order.
3. Every command family in the table maps to a manager already present in `src/main.rs`; the remaining "deferred" rows (`agent send` argv) are tracked in their own follow-up todos under `docs/todo/`.

**Phase 2a — read-only commands** — complete.

- `cargo fmt && cargo clippy --all --benches --tests --examples --all-features` — zero warnings.
- `cargo test -p aura-cli` — 14 parser tests + 11 dispatch smoke tests pass.
- `aura --help` / `aura <family> --help` render; slash `/config file`, `/skills list`, `/channel list` return the same payloads as their argv twins.
- `aura completion zsh > /tmp/_aura && zsh -c 'source /tmp/_aura'` loads without error.
- `aura doctor` reports a warning when `AURA_ALLOW_DEV_ENCRYPTION_KEY=1` and an error when no LLM client is configured.

**Phase 2b — write-mutating commands** — complete. Tracked in `docs/todo/archives/cli-write-commands.md` (archived; one subsystem-level follow-up — `cli-agent-send-argv.md` — carries the remaining deferred work). Each shipped family landed with the following:

- Parser snapshot test in `crates/cli/tests/parser.rs`.
- Dispatch smoke test in `crates/cli/tests/dispatch_smoke.rs`.
- Slash-mode confirmation test (missing `--yes` returns `CliError::ConfirmationRequired`).
- For `agent send`: `printf '/agent send --session x --message hi\n/quit\n' | cargo run` returns `AgentSendForbiddenInSlash`.
