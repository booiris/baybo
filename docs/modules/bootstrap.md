# bootstrap - Entry Point and Configuration Wiring

## Overview

The `aura` binary crate is the sole runtime entry point. It loads `AuraConfig`, translates each section into the corresponding domain type, wires all `Arc`-shared components together, and drives the router until shutdown.

It is **not** a reusable library. Alternative entry points (e.g. integration test harnesses) should either reuse `boot::*` / `runtime::*` directly or graduate the assembly graph into its own crate when a second consumer actually appears — not before.

## Layout

| File | Role |
|------|------|
| `src/main.rs` | Argv-mode dispatch. Parses the CLI, promotes `--config` into `AURA_CONFIG_PATH`, then either short-circuits to a subcommand entry (`gateway_cmd::run`, `setup_cmd::run`, `tui_cmd::run`) or builds a lightweight `CommandContext` and runs `aura_cli::dispatch::run`. |
| `src/boot.rs` | Config → domain translation layer. Pure mappings and small loaders, unit-tested. No `Arc`, no channels, no actor spawning. |
| `src/runtime.rs` | Shared chat-loop assembly: `build_managers`, `wire_router`, `install_signal_handler`, `build_secret_vault`, `force_exit_watchdog`. Used by both the gateway boot path and the TUI's auto-spawn helper. Vault construction goes through `boot::load_encryption_key` directly. |
| `src/gateway_cmd.rs` | Long-running entry point for `aura gateway start` and the supporting installer / token / status subcommands. |
| `src/setup_cmd.rs` | First-run wizard (`aura setup`). |
| `src/tui_cmd.rs` | Interactive `aura tui` entry point: connects to a running gateway over the channel WS. |
| `src/singleton.rs` | Per-workspace `flock` lock acquired by `gateway_cmd::start`. |
| `src/tracing_init.rs` / `src/tui_log.rs` | Tracing setup variants (stdout, file, TUI) plus the in-memory `LogBuffer` and TUI mirror sink. |

## The `boot` module

`boot` is split into two groups:

### Pure mappings (no I/O)

| Function | Maps |
|----------|------|
| `to_execution_policy` | `AgentConfig` → `aura_agent::ExecutionPolicy` |
| `to_token_budget` | `ContextConfig` → `aura_context::TokenBudget` |
| `to_session_timeout` | `SessionConfig` → `chrono::Duration` |
| `to_assessment_mode` | `RiskCheckConfig` → `aura_skills_assessor::AssessmentMode` |
| `build_leak_detector` | `SecurityConfig` → `aura_security::LeakDetector` |
| `storage_db_path` | `WorkspaceConfig` → `PathBuf` at `<workspace.path>/state/storage.db` (the workspace root is itself the aura data directory) |

Each has a unit test in `boot::tests` that pins the mapping. These act as drift detectors: if a config field is renamed or a domain constructor's signature changes, the test fails at compile time.

### Loaders (perform I/O)

| Function | Source | Notes |
|----------|--------|-------|
| `load_config` | `AURA_CONFIG_PATH` → `<default_workspace_root>/config/aura.json` → `Default` | Explicit path that doesn't exist is a hard error; implicit fallback is silent. `default_workspace_root()` is `~/.aura` in release / `<cwd>/.aura` in debug — always absolute, since `AuraConfig::validate` rejects relative `workspace.path`. |
| `resolve_config_path` | Same precedence as `load_config`, returning the path that was used (or `None` for a pure-default boot). | Used by mutation endpoints that need to write `aura.json` back. |
| `build_llm_client` | `default-llm` entry of `AuraConfig`, plus `LlmProviderRegistry`, optional `BlobStore`, optional `SecretVault`, and an `LlmCallGuard` | Delegates credential resolution to `aura_llm::credentials::resolve_api_key`. Returns an `Arc<GuardedLlm>` so every consumer shares the same budget gate. |
| `build_llm_client_for_entry` | Same wiring pinned to a specific non-default `LlmEntry`. | Used by `aura llm probe` / live-model listing. |
| `load_encryption_key` | `security.encryption_key_file` (hex, required) | Rejects non-hex input and any length ≠ 32 bytes. No dev-key fallback — a missing or unreadable file aborts startup rather than silently encrypting secrets with a publicly-known constant. |

Loaders return `anyhow::Result` because they surface I/O and format errors that `ConfigError` deliberately does not model.

## Startup sequence

`src/main.rs` is short and dispatches to subcommand-specific entries:

```
Cli::parse()
  │
  ├─ Commands::Completion       → print_completion(), exit
  ├─ Commands::Gateway { cmd }  → gateway_cmd::run(cmd)
  ├─ Commands::Setup            → setup_cmd::run()
  ├─ Commands::Tui { … }        → tui_cmd::run(config, …)
  └─ everything else            → argv dispatch (lightweight CommandContext + aura_cli::dispatch::run)
```

Long-running paths (`gateway_cmd::start`, `tui_cmd::run`) build their own runtime through `runtime::*`. The chat-loop assembly used by the gateway is:

```
runtime::build_secret_vault            ── opens libsql + master key only
  │
  ▼
mint admin token + fresh TUI token, register on ChannelTokenTable
  │
  ▼
runtime::build_leak_detector(security, gateway_tokens)
init_tracing(File { log_dir, leak_detector })
  │
  ▼
runtime::build_managers(config, shutdown, leak_detector, embedded_mcp_servers)
  │   ── Store::open at <workspace>/state/storage.db
  │   ── SessionManager / JobLifecycle / MemoryManager / CronScheduler / SecurityGateway
  │   ── SkillRegistry / SkillAssessor / ToolRegistry / ToolExecutor / GuardedLlm / CostManager
  │   ── McpReconciler::spawn (re-reads <workspace>/config/.mcp.json on a tick)
  │
  ▼
runtime::wire_router(&mut graph)
  │   ── Router with cron triggers + per-session AgentActor spawner
  │
  ▼
GatewayServer + ChannelServer bound; tokio::select! { admin, channel, router, shutdown }
```

`tui_cmd::run` instead reads the per-start TUI token from the vault, opens a `WsTransport` against the gateway's channel listener, and runs the `TuiAdapter` until shutdown — it does **not** build a manager graph of its own.

The actor-spawner closure passed to `Router::with_actor_spawner` captures clones of all `Arc`-shared state (llm client, tool registry, skill registry, span recorder, policy, tokenizer, token budget, keep-recent, system prompt, mailbox buffer size, cost manager, subagent runtime slot). Any new actor-level dependency must be added to the capture list in `runtime::wire_router`.

## Error handling at boot

| Failure | Outcome |
|---------|---------|
| `AURA_CONFIG_PATH` set but file missing | `bail!` — startup aborts. |
| `<default_workspace_root>/config/aura.json` missing with no env | `info!` + `AuraConfig::default()`. |
| `load_from_file` parse/validate error | `bail!` with full `ConfigError::Validation` list. |
| `boot::load_encryption_key` failure | `bail!` — a missing, unreadable, or malformed `security.encryption_key_file` aborts startup. No fallback: silently encrypting secrets with a constant would be worse than a clear error. |
| `build_llm_client` failure on a chat-loop boot path | `bail!` — unrecoverable, there's no sensible default. Argv-mode commands that don't need the LLM (`channel add`, `config get`, …) downgrade the failure to a `warn!`. |
| Any other `?` at boot | Propagates up, process exits non-zero. |

Fresh checkouts get a usable key from `aura setup`, which mints `<workspace>/.key/encryption.key` (mode 0600) and writes the absolute path into `aura.json`. There is no env-var key, no dev-key fallback, and no other source — exactly one place for the bytes, exactly one place pointed at by the config.

## What boot does NOT do

- **No bootstrap-time MCP wiring beyond launching the reconciler.** MCP servers themselves are configured in `<workspace>/config/.mcp.json`, owned by `aura-tools::mcp` (not `aura-config`). `runtime::build_managers` only spawns the `McpReconciler`; the reconciler reads `.mcp.json` itself on each tick and connects/disconnects servers + registers their tools dynamically.
- **No in-`boot` channel installation.** The `boot` module maps config to types but never touches `ChannelRegistry`. Installation happens one layer out: `runtime::build_managers` calls `aura_gateway::channel::boot::install_channels`, which walks `config.channels` and pre-installs one pinned `Channel` (a registry slot with its own approval gate) per enabled section — `cli`→TUI, `telegram`, `discord`, `weixin` — plus the always-on `http` dashboard channel. So the registry is populated from config at boot, but only with *slots*: each live connection attaches later — TUI/browser/sidecar processes arrive as `/v1/channel-ws` clients, and the `telegram`/`discord`/`weixin` bots are launched and reconciled by the `ChannelBotReconciler` (`aura channel add/remove`).

Neither is future work — both the `McpReconciler` and `install_channels` run at boot today, just from `runtime`/the gateway rather than `boot` itself. `boot`'s job ends at validated config plus mapped primitives; `validate()` having already rejected inconsistent `channels.*` / `cost.*` shapes is what lets that downstream wiring trust them.

## Constraints

- `boot` depends on `aura-config` and the domain crates it translates into — nothing else.
- `runtime` and the per-subcommand entrypoints own `Arc` lifetime management; `boot` must not hold shared state.
- Pure-mapping functions must be pure: no allocations the caller can't see, no env reads, no filesystem access.
- Every new config field that flows into runtime state must go through `boot::*` and gain a unit test covering the mapping.

## Collaboration

| Module | Role |
|--------|------|
| `config` | Provides `AuraConfig` and section types. `boot` consumes them. |
| `agent` | Consumes `ExecutionPolicy`, `SessionManager`, `JobLifecycle`, `CostManager`, `SecretVault`, etc. assembled by `runtime::build_managers`. |
| `llm`, `context`, `security` | Each exposes the constructor that a `boot::to_*` or `boot::build_*` function targets. |
| Other crates | Never called from `boot` directly; they are assembled by `runtime` after `boot` has produced the required primitives. |
