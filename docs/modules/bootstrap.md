# bootstrap - Entry Point and Configuration Wiring

## Overview

The `aura` binary crate (`src/main.rs` + `src/boot.rs`) is the sole runtime entry point. It loads `AuraConfig`, translates each section into the corresponding domain type, wires all `Arc`-shared components together, and drives the router until shutdown.

It is **not** a reusable library. Alternative entry points (e.g. integration test harnesses) should either reuse `boot::*` directly or graduate the assembly graph into its own crate when a second consumer actually appears — not before.

## Layout

| File | Role |
|------|------|
| `src/main.rs` | Startup choreography: storage → managers → registries → router → signal handling. Holds all `Arc` wiring and closures. |
| `src/boot.rs` | Config → domain translation layer. Pure mappings and small loaders, unit-tested. No `Arc`, no channels, no actor spawning. |

## The `boot` module

`boot` is split into two groups:

### Pure mappings (no I/O)

| Function | Maps |
|----------|------|
| `to_execution_policy` | `AgentConfig` → `aura_agent::ExecutionPolicy` |
| `to_token_budget` | `ContextConfig` → `aura_context::TokenBudget` |
| `to_session_timeout` | `SessionConfig` → `chrono::Duration` |
| `to_tool_timeout` | `ToolsConfig` → `std::time::Duration` |
| `build_leak_detector` | `SecurityConfig` → `aura_security::LeakDetector` |
| `storage_db_path` | `WorkspaceConfig` → `PathBuf` at `<workspace.path>/storage.db` (the workspace root is itself the aura data directory) |

Each has a unit test in `boot::tests` that pins the mapping. These act as drift detectors: if a config field is renamed or a domain constructor's signature changes, the test fails at compile time.

### Loaders (perform I/O)

| Function | Source | Notes |
|----------|--------|-------|
| `load_config` | `AURA_CONFIG_PATH` → `./aura.json` → `Default` | Explicit path that doesn't exist is a hard error; implicit fallback is silent. |
| `build_llm_client` | `LlmConfig` + env var | Uses `LlmProviderRegistry::with_default_providers()`. |
| `resolve_llm_api_key` | `cfg.api_key_env`, else `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | Returns `None` when nothing is set; the provider factory decides whether that's acceptable. |
| `load_encryption_key` | `security.encryption_key_file` (hex) or `security.encryption_key_env` (hex) | Rejects non-hex input and any length ≠ 32 bytes. |

Loaders return `anyhow::Result` because they surface I/O and format errors that `ConfigError` deliberately does not model.

## Startup sequence (main.rs)

```
init_tracing()
  │
  ▼
boot::load_config()                          ── AURA_CONFIG_PATH / aura.json / default
  │
  ▼
Store::open(boot::storage_db_path(…))        ── persistent libsql at <workspace>/storage.db
  │
  ▼
SessionManager / JobManager / CostTracker / TraceCollector / ObservabilityRecorder
  │
  ▼
boot::load_encryption_key  →  SecretVault    ── dev-only fallback gated on AURA_ALLOW_DEV_ENCRYPTION_KEY=1
  │
  ▼
ToolRegistry / ToolExecutor / MemoryManager / LlmClient
  │
  ▼
WorkspaceManager / Soul / ExecutionPolicy / LeakDetector / SecurityGateway
  │
  ▼
ChannelRegistry::new()            ── empty at boot; populated by WS sidecars
  │
  ▼
Router::new(…).with_actor_spawner(closure).with_cron_triggers(…)
  │
  ▼
tokio::select! { router.run(…), shutdown.wait() }
```

The closure passed to `with_actor_spawner` captures clones of all `Arc`-shared state (llm client, tool registry, skill registry, recorder, policy, tokenizer, token budget, keep-recent, system prompt, mailbox buffer size). Any new actor-level dependency must be added to the capture list.

## Error handling at boot

| Failure | Outcome |
|---------|---------|
| `AURA_CONFIG_PATH` set but file missing | `bail!` — startup aborts. |
| `aura.json` missing with no env | `info!` + `AuraConfig::default()`. |
| `load_from_file` parse/validate error | `bail!` with full `ConfigError::Validation` list. |
| `load_encryption_key` failure | `bail!` unless `AURA_ALLOW_DEV_ENCRYPTION_KEY=1` is set. With the flag, `error!` + dev-only `b"aura-dev-master-key-32-bytes-ok!"` fallback. Must not ship to production. |
| `build_llm_client` failure | `bail!` — unrecoverable, there's no sensible default. |
| Any other `?` at boot | Propagates up, process exits non-zero. |

The dev fallback for the encryption key is intentional but explicit: a fresh checkout runs with `AURA_ALLOW_DEV_ENCRYPTION_KEY=1 cargo run`. Without the flag, a missing or mistyped key source aborts startup rather than silently encrypting secrets with a publicly-known key. When the flag is honoured, an `error!` line fires on every boot so it cannot be mistaken for a working setup.

## What boot does NOT do

- **No bootstrap-time MCP wiring beyond launching the reconciler.** MCP servers themselves are configured in `<workspace>/.mcp.json`, owned by `aura-tools::mcp` (not `aura-config`). `runtime::build_managers` only spawns the `McpReconciler`; the reconciler reads `.mcp.json` itself on each tick and connects/disconnects servers + registers their tools dynamically.
- **No compiled-in channel adapters.** `ChannelRegistry` starts empty. Every channel — the bundled TUI and any sidecar plugin — arrives at runtime as a `/v1/channel-ws` client and registers itself from the gateway's route task. `channels.{http, telegram, discord}` in `aura.json` are validated but not yet wired.
- **No cost guard or rate limiter** — `cost.*` is validated but not yet consumed by the running router.

These are spec'd in `config.md` and are future wiring work; `validate()` already rejects inconsistent configurations for them so later wiring can trust the shapes.

## Constraints

- `boot` depends on `aura-config` and the domain crates it translates into — nothing else.
- `main.rs` owns `Arc` lifetime management; `boot` must not hold shared state.
- Pure-mapping functions must be pure: no allocations the caller can't see, no env reads, no filesystem access.
- Every new config field that flows into runtime state must go through `boot::*` and gain a unit test covering the mapping.

## Collaboration

| Module | Role |
|--------|------|
| `config` | Provides `AuraConfig` and section types. `boot` consumes them. |
| `agent` | Consumes `ExecutionPolicy`, `SessionManager`, `TraceCollector`, `CostTracker`, `SecretVault`, etc. assembled by `main.rs`. |
| `llm`, `context`, `security` | Each exposes the constructor that a `boot::to_*` or `boot::build_*` function targets. |
| Other crates | Never called from `boot` directly; they are assembled by `main.rs` after `boot` has produced the required primitives. |
