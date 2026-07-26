# bootstrap - Entry Point and Configuration Wiring

## Overview

The `baybo` binary crate is the sole runtime entry point. It loads `BayboConfig`, translates each section into the corresponding domain type, wires all `Arc`-shared components together, and drives the router until shutdown.

It is **not** a reusable library. Alternative entry points (e.g. integration test harnesses) should either reuse `boot::*` / `runtime::*` directly or graduate the assembly graph into its own crate when a second consumer actually appears — not before.

## Layout

| File | Role |
|------|------|
| `crates/baybo/src/main.rs` | Argv-mode dispatch. Parses the CLI, promotes `--config` into `BAYBO_CONFIG_PATH`, then either prints help for a bare `baybo`, short-circuits to a subcommand entry (`gateway_cmd::run`, `setup_cmd::run`, `prompt_cmd::run`, `tui_cmd::run`), or builds a lightweight `CommandContext` and runs `baybo_cli::dispatch::run`. |
| `crates/baybo/src/boot.rs` | Config → domain translation layer. Pure mappings and small loaders, unit-tested. No `Arc`, no channels, no actor spawning. |
| `crates/baybo/src/runtime.rs` | Shared chat-loop assembly: `build_managers`, `wire_router`, `install_signal_handler`, `build_secret_vault`, `force_exit_watchdog`. Used by the gateway boot path and by `baybo prompt`'s in-process (no-gateway) fallback; the TUI only borrows the small helpers (`build_secret_vault`, `install_signal_handler`, `force_exit_watchdog`). Vault construction goes through `boot::load_encryption_key` directly. |
| `crates/baybo/src/sandbox_boot.rs` | Boot-time Bash sandbox policy: bench skip, outer-container detection, backend warm-up, and downgrade reason selection. |
| `crates/baybo/src/gateway_cmd.rs` | Long-running entry point for `baybo gateway start` and the supporting installer / token / status subcommands. |
| `crates/baybo/src/setup_cmd.rs` | First-run wizard (`baybo setup`). |
| `crates/baybo/src/tui_cmd.rs` | Interactive `baybo tui` entry point: connects to a running gateway over the channel WS. |
| `crates/baybo/src/prompt_cmd.rs` | Headless one-shot `baybo prompt`. Keys off the per-workspace singleton lock: a live gateway holds it → route the turn over WS; the lock is free → acquire it and build the agent runtime in-process for this one turn via `runtime::build_managers` / `runtime::wire_router`. |
| `crates/baybo/src/gateway_client.rs` | Shared dial path for the WS channel clients (`tui_cmd`, `prompt_cmd`): resolves the gateway's admin listener from config, reads the per-start TUI token from the vault, and connects `WsTransport` to `/v1/channel-ws`. |
| `crates/baybo/src/reload.rs` | In-process config hot-reload orchestrator. Implements `baybo_gateway::ConfigReloader` with a two-phase prepare→commit swap; lives here because rebuilding the LLM pool needs `boot::build_llm_client_for_entry`. |
| `crates/baybo/src/singleton.rs` | Per-workspace `flock` lock acquired by `gateway_cmd::start`. |
| `crates/baybo/src/tracing_init.rs` / `crates/baybo/src/tui_log.rs` | Tracing setup variants (stdout, stderr, file, TUI) plus the in-memory `LogBuffer` and TUI mirror sink. |

## The `boot` module

`boot` is split into two groups:

### Pure mappings (no I/O)

| Function | Maps |
|----------|------|
| `to_assessment_mode` | `RiskCheckConfig` → `baybo_skills_assessor::AssessmentMode` |
| `to_bash_permission` | `baybo_config::PermissionPolicy` → `baybo_tools::builtin::BashPermissionMode` (shared by initial wiring and hot-reload) |
| `proxy_settings` | `BayboConfig` → `Option<baybo_security::http::ProxySettings>` (`None` = direct connections) |
| `build_leak_detector` | `SecurityConfig` → `baybo_security::LeakDetector` (the base detector). A second `runtime::build_leak_detector(security, gateway_tokens)` wraps this one to also add per-token redaction rules for the live gateway tokens; `boot`'s version is the config-only base. |
| `storage_db_path` | `WorkspaceConfig` → `PathBuf` at `<workspace.path>/state/storage.db` (the workspace root is itself the baybo data directory) |

Larger domain objects (`AgentLoop`, `ContextManager`, `Router`) are no longer built through a `boot::to_*` mapping; they are assembled in `runtime.rs` via each type's `from_config` constructor (`AgentLoop::from_config`, `ContextManager::from_config`, `Router::from_config`), populating a sibling `XxxConfig` struct literal so every required field shows up by name at the call site.

`storage_db_path` and `build_leak_detector` each have a unit test in `boot::tests` that pins the mapping. These act as drift detectors: if a config field is renamed or a domain constructor's signature changes, the test fails at compile time.

### Loaders (perform I/O)

| Function | Source | Notes |
|----------|--------|-------|
| `load_config` | `BAYBO_CONFIG_PATH` → `<default_workspace_root>/config/baybo.json` → `Default` | Explicit path that doesn't exist is a hard error; implicit fallback is silent. `default_workspace_root()` is `~/.baybo` in release / `<cwd>/.baybo` in debug — always absolute, since `BayboConfig::validate` rejects relative `workspace.path`. |
| `resolve_config_path` | Same precedence as `load_config`, returning the path that was used (or `None` for a pure-default boot). | Used by mutation endpoints that need to write `baybo.json` back. |
| `build_llm_client` | `default-llm` entry of `BayboConfig`, plus `LlmProviderRegistry`, optional `BlobStore`, optional `SecretVault`, and `CostHooks` (the `LlmCallGuard` admission gate bundled with the post-call `LlmCostRecorder`; `CostHooks::passthrough()` for unbilled one-shots) | Delegates credential resolution to `baybo_llm::credentials::resolve_api_key`. Returns an `Arc<BillableLlm>` so every consumer shares the same budget gate. |
| `build_llm_client_for_entry` | Same wiring pinned to a specific non-default `LlmEntry`. | Used by `baybo llm probe` / live-model listing. |
| `load_encryption_key` | `security.encryption_key_file` (hex, required) **and the secret store** | Rejects non-hex input and any length ≠ 32 bytes. No dev-key fallback — a missing or unreadable file aborts startup rather than silently encrypting secrets with a publicly-known constant. Takes the store because it delegates to `baybo_security::key_file::resolve_pending`, which completes or discards an interrupted key rotation; deciding which key is live means trying to decrypt a real vault entry, not reading on-disk bookkeeping. Every path that builds a `SecretVault` must go through here — one that read the file directly would come up with the pre-rotation key and fail every decrypt. |

Loaders return `anyhow::Result` because they surface I/O and format errors that `ConfigError` deliberately does not model.

## Startup sequence

`crates/baybo/src/main.rs` is short and dispatches to subcommand-specific entries:

```
Cli::parse()
  │
  ├─ Commands::Completion       → print_completion(), exit
  ├─ Commands::Gateway { cmd }  → gateway_cmd::run(cmd)
  ├─ Commands::Setup            → setup_cmd::run()
  ├─ (no subcommand)            → print help, exit
  ├─ Commands::Prompt { … }     → prompt_cmd::run(config, …)
  ├─ Commands::Tui { … }        → tui_cmd::run(config, …)
  └─ everything else            → argv dispatch (lightweight CommandContext + baybo_cli::dispatch::run)
```

Long-running paths (`gateway_cmd::start`, and `prompt_cmd::run` when no gateway holds the workspace lock) build their own runtime through `runtime::*`. The chat-loop assembly used by the gateway is:

```
runtime::build_secret_vault            ── opens sqlite + master key only
  │
  ▼
mint admin token + fresh TUI token, register on ChannelTokenTable
  │
  ▼
runtime::build_leak_detector(security, gateway_tokens)
init_tracing(File { log_dir, leak_detector })
  │
  ▼
runtime::build_managers(config, config_path, shutdown, leak_detector, embedded_mcp_servers)
  │   ── config_path feeds the hot-reload orchestrator; `None` disables reload
  │   ── Store::open at <workspace>/state/storage.db
  │   ── SessionManager / JobLifecycle / CronScheduler / SecurityGateway
  │   ── SkillRegistry / SkillAssessor / ToolRegistry / ToolExecutor / BillableLlm / CostManager
  │   ── McpReconciler::spawn (re-reads <workspace>/config/.mcp.json on a tick)
  │
  ▼
runtime::wire_router(&mut graph)
  │   ── Router with cron triggers + per-session AgentActor spawner
  │
  ▼
GatewayServer + ChannelServer bound; tokio::select! { admin, channel, router, shutdown }
```

`tui_cmd::run` instead reads the per-start TUI token from the vault, opens a `WsTransport` against the gateway's admin listener (which co-hosts the `/v1/channel-ws` upgrade; no port-file discovery), and runs the `TuiAdapter` until shutdown — it does **not** build a manager graph of its own.

The actor-spawner closure handed to `Router::from_config` via `RouterConfig::actor_spawner` captures clones of all `Arc`-shared state (LLM pool, tool + skill registries, tool executor, trace and task stores, job lifecycle, security gateway, tokenizer, trace event stream, token calibration, session manager, subagent registry, workspace paths, supervisor, memory, title sink, plus max-iterations / compression-threshold / keep-recent). Any new actor-level dependency must be added to the capture list in `runtime::wire_router`.

## Error handling at boot

| Failure | Outcome |
|---------|---------|
| `BAYBO_CONFIG_PATH` set but file missing | `bail!` — startup aborts. |
| `<default_workspace_root>/config/baybo.json` missing with no env | `info!` + `BayboConfig::default()`. |
| `load_from_file` parse/validate error | `bail!` with full `ConfigError::Validation` list. |
| `boot::load_encryption_key` failure | `bail!` — a missing, unreadable, or malformed `security.encryption_key_file` aborts startup. No fallback: silently encrypting secrets with a constant would be worse than a clear error. Also fails when neither the live nor a leftover pending key opens the vault, which means an interrupted rotation lost both halves — starting anyway would give a process that decrypts nothing. |
| `build_llm_client` failure on a chat-loop boot path | `bail!` — unrecoverable, there's no sensible default. Argv mode only builds the client for commands whose handlers read `ctx.llm` (`doctor`, `status` — see `needs_llm`), and those downgrade the failure to a `warn!`; everything else (`channel add`, `config get`, …) skips the build entirely. |
| Any other `?` at boot | Propagates up, process exits non-zero. |

Fresh checkouts get a usable key from `baybo setup`, which mints `<workspace>/.key/encryption.key` (mode 0600) and writes the absolute path into `baybo.json`. There is no env-var key, no dev-key fallback, and no other source — exactly one place for the bytes, exactly one place pointed at by the config.

## What boot does NOT do

- **No bootstrap-time MCP wiring beyond launching the reconciler.** MCP servers themselves are configured in `<workspace>/config/.mcp.json`, owned by `baybo-tools::mcp` (not `baybo-config`). `runtime::build_managers` only spawns the `McpReconciler`; the reconciler reads `.mcp.json` itself on each tick and connects/disconnects servers + registers their tools dynamically.
- **No in-`boot` channel installation.** The `boot` module maps config to types but never touches `ChannelRegistry`. Installation happens one layer out: `runtime::build_managers` calls `baybo_gateway::channel::boot::install_channels`, which walks `config.channels` and pre-installs one pinned `Channel` (a registry slot with its own approval gate) per enabled section — `cli`→TUI, `telegram`, `discord`, `weixin` — plus the always-on `owner` chat channel (the shared web-dashboard + paired-mobile surface; both register as `owner`, and the web/device auth gates are what admit a connection). So the registry is populated from config at boot, but only with *slots*: each live connection attaches later — TUI/browser/sidecar processes and paired mobile clients arrive as `/v1/channel-ws` clients, and the `telegram`/`discord`/`weixin` bots are launched and reconciled by the `ChannelBotReconciler` (`baybo channel add/remove`).

Neither is future work — both the `McpReconciler` and `install_channels` run at boot today, just from `runtime`/the gateway rather than `boot` itself. `boot`'s job ends at validated config plus mapped primitives; `validate()` having already rejected inconsistent `channels.*` / `cost.*` shapes is what lets that downstream wiring trust them.

## Constraints

- `boot` depends on `baybo-config` and the domain crates it translates into — nothing else.
- `runtime` and the per-subcommand entrypoints own `Arc` lifetime management; `boot` must not hold shared state.
- Pure-mapping functions must be pure: no allocations the caller can't see, no env reads, no filesystem access.
- Every new config field that flows into runtime state must go through `boot::*` and gain a unit test covering the mapping.

## Collaboration

| Module | Role |
|--------|------|
| `config` | Provides `BayboConfig` and section types. `boot` consumes them. |
| `agent` | Consumes `SecurityGateway`, `SessionManager`, `JobLifecycle`, `CostManager`, `SecretVault`, etc. assembled by `runtime::build_managers`. |
| `llm`, `context`, `security` | Each exposes the constructor that a `boot::to_*` or `boot::build_*` function targets. |
| Other crates | Never called from `boot` directly; they are assembled by `runtime` after `boot` has produced the required primitives. |
