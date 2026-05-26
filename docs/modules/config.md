# config - Unified Configuration Loading and Validation

## Overview

The `config` crate owns the root `AuraConfig` struct, JSON loading, and the `validate()` method. It centralizes settings that were previously scattered across individual crates or hardcoded in `main.rs` (context-window budget, channel buffer sizes, rate limits, etc.).

A single JSON file — typically `aura.json` — maps 1:1 to `AuraConfig`. Consumers (`main.rs` and `aura-agent`) map each section into the corresponding domain type.

Top-level sections: `llm` (a `Vec<LlmEntry>`) plus `default-llm: LlmEntryName`, `agent`, `channels`, `security`, `skills`, `cost`, `workspace`, `gateway`, `browser`, `external_agents`.

> **MCP status note.** MCP server records do **not** live in `aura.json`.
> They live in `<workspace>/config/.mcp.json`, owned by `aura-tools::mcp`
> (config shape: `McpFile`, `McpServerEntry`, `McpTransportConfig`,
> `OAuthConfig`, plus its own `TrustLevelConfig`). Per-tool execution
> timeouts are declared by each tool itself via `Tool::max_timeout`
> (defaults to 30 s). See `docs/modules/tools.md` for the MCP client
> architecture and per-tool timeout overrides, and `docs/modules/cli.md`
> for the `aura mcp {add,list,get,remove}` surface.

There is no `storage` section. Storage paths are **derived** from the project root (`workspace.path`) — operators choose a project root, not individual data-file locations.

## Design Decisions

### Leaf-level placement in the dependency graph

The crate sits near the leaf of the dependency graph: its only runtime `aura-*` deps are `aura-model` (for shared newtypes the config surface reuses directly — `LlmEntryName`, `ModelTier`, `MicroUsd`, `ExternalAgentKind`) and `aura-workspace` (paths only, pulled in with `default-features = false` to keep the I/O layer out of the dep graph). It otherwise depends on external libraries only — `serde`, `serde_json`, `tokio`, `thiserror`, `parking_lot`. (`aura-tools` is a dev-dependency, used solely by the mirror contract tests.) This keeps the surface small and deliberate:

- Avoids coupling the config surface to most domain type changes
- Keeps `config` cheap to build, low in the graph
- Prevents circular dependencies when `agent` wants to read configuration

To compensate, `config` defines **mirror structs** for domain types it references (e.g., `TrustLevelConfig` in `aura-config::tools` mirrors `aura_model::TrustLevel`). Mapping between mirror and domain types happens at the consumer (startup code or `agent` bootstrap). See §"Mirror maintenance contract" for drift prevention. The MCP-specific mirrors (`McpServerEntry`, `McpTransportConfig`, `OAuthConfig`, plus a second `TrustLevelConfig`) live in `aura-tools::mcp::config` because MCP server records are persisted in `<workspace>/config/.mcp.json` rather than `aura.json`.

### Defaults-first serde strategy (top-level only)

Every **top-level** section carries `#[serde(default)]` and a matching `Default` impl. An empty JSON object `{}` deserializes into a fully valid `AuraConfig`; users only specify fields they want to override.

This does **not** extend uniformly into nested structs. The following nested types have required serde fields — supplying the parent object without them fails at deserialization, not in `validate()`:

- `TelegramChannelConfig` (`enabled`, `bot_token_env`) — under `channels.telegram`
- `DiscordChannelConfig` (`enabled`, `bot_token_env`) — under `channels.discord`
- `LlmEntry` (`name`, `provider`, `model`) — every element of the top-level `llm` array

Required-ness beyond serde (non-empty strings, numeric ranges, URL schemes) is enforced by `validate()`.

### Collect-all validation, not fail-fast

`AuraConfig::validate()` walks every section and accumulates all `ValidationError` entries before returning. The returned `ConfigError::Validation(Vec<ValidationError>)` surfaces every problem at once so users can fix the entire file in one pass rather than iterating on single errors. `AuraConfig::load_from_str` and `load_from_file` call `validate()` internally — callers do not need to invoke it separately.

### JSON format (not TOML or YAML)

JSON is the sole supported format. It has the widest tooling support, round-trips through `serde_json`, and matches the project's existing use of JSON for trace payloads.

### Unknown fields

`serde`'s default tolerance applies: unknown keys are silently ignored. This is permissive by design so a newer JSON file (with fields an older binary does not yet know about) does not hard-fail at load. The cost is that typos in field names are also silent — `"agent": { "max_iteration": 10 }` parses fine and the real `max_iterations` stays at its default.

Sections that must not accept typos (security-sensitive or governance-sensitive shapes, e.g. `security` and — once reintroduced — `tools.mcp_servers[]`) may opt into `#[serde(deny_unknown_fields)]` individually. The root `AuraConfig` intentionally keeps permissive semantics.

### Secret handling

Config does **not** store live secret values; it stores references:

- `LlmEntry::api_key_env` is a reference to an env-var name (e.g., `"OPENAI_API_KEY"`), not raw key material. `llm.md` §Constraints prohibits inline keys. When absent, `aura_llm::credentials::resolve_api_key` falls back to the per-entry vault key (`llm.entry.<name>.api_key`) and then to provider-specific defaults (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `MINIMAX_API_KEY`).
- `SecurityConfig::encryption_key_file` is the only encryption-key source: an absolute path to a hex-encoded 32-byte file (mode 0600). `aura setup` mints one at `<workspace>/.key/encryption.key`. A missing or unreadable file is a hard error at startup — there is no env-var alternative and no dev-key fallback.

### Section boundaries

Sections mirror Aura's real runtime concerns, not a 1:1 copy of any external reference:

| Section    | Maps to                                                     | Notes                                                                                                                                                                                                |
| ---------- | ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `llm`      | `Vec<LlmEntry>` + `default-llm: String` → `aura_llm::LlmProviderConfig` | Each `LlmEntry` is a `{name, provider, model, api_key_env?, base_url?, supports_vision?, reasoning_effort?}` record. `default-llm` names the entry the agent loop uses by default; the field is serde-renamed to `default-llm`. Multiple entries can target the same provider with distinct credentials. |
| `agent`    | `ExecutionPolicy` + `TokenBudget` + `Truncate::keep_recent` + subagent caps + tier map | Carries `max_iterations`, the context-window budget, `max_subagent_depth`, `max_subagents_per_root`, and `model_tiers` (`ModelTier` → `LlmEntryName`). Per-tool timeouts are not configured here — each `Tool` impl declares its own ceiling via `Tool::max_timeout` (default 30 s). |
| `channels` | `ChannelRegistry` adapter enablement + mpsc buffer sizes    | See §"Channel enablement model".                                                                                                                                                                     |
| `security` | `EncryptionKey` location + `LeakDetector` enablement        |                                                                                                                                                                                                      |
| `skills`   | `aura_skills_assessor::AssessmentMode`                      | `risk_check`: `off` disables the LLM classifier, `primary` (default) judges `SKILL.md` only, `full` judges the whole directory tree.                                                                 |
| `cost`     | `SpendingLimits` + `Router::with_rate_limit`                |                                                                                                                                                                                                      |
| `workspace`| `WorkspaceManager` + storage path composition               | Single field: `path`. The project root from which all persistent data paths are composed (e.g. `<workspace.path>/state/storage.db`).                                                                |
| `gateway`  | `aura_gateway::RuntimeGatewayConfig`                        | Admin bind address + port, CORS allowlist, shutdown grace. See [`gateway.md`](gateway.md).                                                                                                          |
| `browser`  | `aura_tools::browser` configuration                         | Browser sidecar launch settings (docker mode, profile path).                                                                                                                                         |
| `external_agents` | `aura_agent::external_agent` registry                | Per-kind opt-in for the host-execution external agents — `claude`, `codex`, `gemini` (each `{ enabled, binary_path? }`), plus an optional `default_external_agent` that designates the primary when more than one is enabled. `enabled` defaults to `false`: an installed binary on PATH does not auto-grant access. |

`registry` and `cron` currently have no top-level section. See §"Out-of-scope modules" for rationale and planned placement.

### Channel enablement model

Each optional channel (`telegram`, `discord`, `weixin`) is wrapped in `Option<_>`: **absent ⇒ disabled, present ⇒ enabled**. The inner `enabled: bool` is redundant with the `Option` wrapper and is retained only for migration. `validate()` enforces this self-consistency for the two token-backed channels — `validate_channels` rejects `Some { enabled: false, ... }` for `telegram` and `discord` and guides the operator to omit the section instead. `TelegramChannelConfig` and `DiscordChannelConfig` each carry an additional required `bot_token_env`; `WeixinChannelConfig` has only `enabled: bool` (no token field) and is not currently inspected by `validate()`. The `cli` channel is always present because it has no required configuration and ships as the default adapter. The `http` channel is likewise always installed — it powers the embedded web dashboard / chat page, has no operator-facing knobs, and so carries no `channels` section of its own.

## Out-of-scope modules

The following modules do not (yet) have sections in the root config. This is a deliberate phased decision, not an oversight. Each has a planned placement:

- **registry** — artifact source allowlist, signature verification policy, trust ceilings. Today the defaults are baked into the registry constructors.
- **cron** — scheduler poll interval, max concurrent runs, missed-run policy. Today `CronScheduler` uses compile-time defaults.

Principle: a module earns a config section when operators need to tune it in production. Until that need appears, keeping the surface small avoids defaults sprawl.

## Mirror maintenance contract

`aura-config` holds mirrors of selected domain types (today, `TrustLevelConfig` in `aura-config::tools` mirrors `aura_model::TrustLevel`). The MCP-specific mirrors (`McpServerEntry`, `McpTransportConfig`, `OAuthConfig`, plus a second `TrustLevelConfig`) live in `aura-tools::mcp::config`, not here, because `.mcp.json` is owned by `aura-tools`. Drift prevention applies to both crates:

1. **Ownership** — mirrors live in `aura-config`. Whenever the upstream domain type (e.g. `aura_model::TrustLevel`) changes shape, the same PR updates the mirror and the conversion between them.
2. **Contract tests** — each mirror has a round-trip test (`From<DomainType> for MirrorType` and `TryFrom<MirrorType> for DomainType`) in `aura-config`'s integration tests. These act as the drift detector: adding a variant upstream without a mirror update breaks match exhaustiveness and fails CI.
3. **Forward compatibility** — domain enums that mirrors target should be `#[non_exhaustive]`; the mirror's `TryFrom` returns a typed `ConfigError::UnsupportedVariant { ty, variant }` rather than panicking when it encounters an unknown variant.
4. **Scope limit** — only types that appear in the config surface are mirrored. Transient/internal domain types must not leak into `aura-config`.

## Reload semantics

`aura-config` ships the reload **primitives**, not the orchestration. As a leaf crate it owns two pure pieces in `reload.rs`: a live, swappable handle to the applied config (`ConfigHandle`) and the whitelist gate (`hot_reload_diff`). The fallible derived-state rebuilds (the LLM pool, cost limits) and the end-to-end reload flow live in consumer crates — see [`docs/config-hot-reload.md`](../config-hot-reload.md) before touching reload code. The contract below is the part `aura-config` itself enforces.

- **Live handle** — `ConfigHandle` wraps `Arc<parking_lot::RwLock<Arc<AuraConfig>>>`. `current()` clones out the applied `Arc`; `store()` is the infallible commit half that swaps a new `Arc` in. Reads happen per-turn / per-request (resolving the active model, dashboard reads), never per-token, so a plain `RwLock<Arc<_>>` is ample — no `ArcSwap` dependency. The previous `Arc` stays alive until its last in-flight reader drops it, which gives the "in-flight requests finish on the old config" behaviour below.
- **Hot-updatable whitelist** — `hot_reload_diff(old, new)` enforces an explicit allowlist: `llm`, `default_llm`, `agent.model_tiers`, `cost.rate_limit`, `cost.spending_limits`. Any reload whose diff touches a field **outside** this set hard-rejects the entire reload (atomic — nothing swaps) with `ConfigError::NotHotReloadable { section }` naming the offending section. Not hot-updatable: `gateway.*`, `workspace.path`, `security.*`, `channels.*`, `external_agents.*`, and the rest of `agent` (`max_iterations`, `context`, `max_subagent_depth`, `max_subagents_per_root`). `new` is destructured field-by-field so adding a field to `AuraConfig` or `AgentConfig` forces a hot/non-hot classification here rather than silently defaulting to "hot, unchecked".
- **Atomic swap** — a successful reload swaps a single `Arc<AuraConfig>` holding all whitelisted changes together. Partial application is forbidden.
- **Validation rollback** — a reload that fails `validate()` leaves the running config untouched and returns `ConfigError` to the caller; no partial state is exposed.
- **In-flight behavior** — requests already running against the old config continue with its values; only new requests pick up the new config. For LLM turns this is per-turn: a turn finishes on the client it resolved at turn start.
- **Fallible derived rebuilds** — a consumer whose live state is fallible to rebuild (the LLM pool: the default entry can fail to build) gates the swap via a two-phase prepare→commit. A failed prepare aborts before anything swaps.

## Validation Rules

### Field-level rules

| Section                               | Rule                                                 |
| ------------------------------------- | ---------------------------------------------------- |
| `llm[i].name`                         | non-empty; unique within `llm`                       |
| `llm[i].provider`                     | non-empty                                            |
| `llm[i].model`                        | non-empty                                            |
| `llm[i].base_url`                     | if set, scheme is `http://` or `https://`            |
| `llm[i].api_key_env`                  | if set, valid env-var identifier                     |
| `default-llm`                         | when `llm` is non-empty, must name an existing entry |
| `agent.max_iterations`                | in `1..=1000`                                        |
| `agent.context.compression_threshold` | in `(0.0, 1.0]`, finite                              |
| `agent.context.keep_recent`           | ≥ 1                                                  |
| `agent.max_subagent_depth`            | ≤ 32                                                 |
| `agent.max_subagents_per_root`        | in `1..=256`                                         |
| `workspace.path`                      | non-empty                                            |
| `channels.message_buffer_size`        | in `1..=65536`                                       |
| `channels.telegram.bot_token_env`     | non-empty                                            |
| `channels.discord.bot_token_env`      | non-empty                                            |
| `cost.spending_limits.daily_usd`      | if set, strictly positive, finite                    |
| `cost.spending_limits.monthly_usd`    | if set, strictly positive, finite                    |
| `cost.spending_limits`                | cross-field: `daily_usd ≤ monthly_usd` when both set |
| `cost.rate_limit.*`                   | `max_requests ≥ 1`, `window_secs ≥ 1`                |

### Cross-section rules

Field-level checks catch syntax errors; cross-section checks catch policy inconsistencies. These live in a dedicated `validate_cross_section(&self, ...)` pass that runs after the per-section passes complete:

| Rule                                                                                                                                                                             | Sections involved  |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------ |
| `default-llm` (when `llm` is non-empty) must reference an existing `llm[i].name`                                                                                                 | `llm`              |
| Each `agent.model_tiers` target must name an existing `llm[i].name`                                                                                                             | `agent` / `llm`    |
| When more than one `external_agents` kind is `enabled`, `default_external_agent` must be set and name one of the enabled kinds                                                   | `external_agents`  |
| `channels.telegram` / `channels.discord` with `enabled: false` is rejected (enablement-model self-consistency)                                                                  | `channels`         |
| `security.encryption_key_file` must be set and absolute (string path to a hex-encoded 32-byte file)                                                                              | `security`         |
| `workspace.path`, `security.encryption_key_file`, `browser.chrome_path`, and `browser.profile_dir` must be absolute paths (no `./`, no `~`)                                      | `workspace` / `security` / `browser` |

The MCP-specific trust/capability rules (stdio requires `trusted`, `installed`/`untrusted` may not declare `WriteFile`/`ExecCommand`) live with the MCP file in `aura-tools::mcp::config` since `.mcp.json` is owned there.

Cross-section rules are part of the default `validate()` pass. A future strict-load flag will also enforce advisory hygiene (e.g. key-file extension hints, env-var name syntax); today those are handled case-by-case in bootstrap.

## Constraints

- Near-leaf in the dependency graph: the only runtime `aura-*` deps are `aura-model` (shared newtypes the config reuses) and `aura-workspace` (paths only). No dependency on the heavier domain crates (`agent`, `llm`, `tools`, …)
- No secret plaintext in the config struct; only references (env var names, file paths)
- Validation must be pure — no I/O, no time-of-day dependencies, no filesystem probes
- All top-level sections must provide a `Default` impl whose output passes `validate()`
- Mirrors of domain types must satisfy §"Mirror maintenance contract"
- Runtime config mutations go through the §"Reload semantics" primitives (`ConfigHandle` + `hot_reload_diff`); only the whitelisted sections may change live, and the orchestration lives in consumer crates

## Collaboration

| Module     | Role                                                                                                                                                                                   |
| ---------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `main.rs`  | Loads the config at startup, maps each section into domain types, passes them down                                                                                                     |
| `agent`    | Consumes `AgentConfig`, `CostConfig`, `ExternalAgentsConfig`                                                                                                                            |
| `llm`      | Receives `LlmProviderConfig` built from each `LlmEntry`                                                                                                                                |
| `tools`    | No `aura.json` section: per-tool timeouts come from `Tool::max_timeout`. (Once MCP lands again, the workspace-local `.mcp.json` continues to own MCP server records.)                                 |
| `channels` | Channel adapters are registered based on `ChannelsConfig` section enablement                                                                                                           |
| `hook`     | `ConfigChange` is an extension point that _observes_ or _vetoes_ proposed changes. It does **not** emit provenance — provenance is recorded by the bootstrap/agent layer into `trace`. |
| `trace`    | Records `provider_config_hash` / `config_version` in `ExecutionProvenance` when config is loaded or changed                                                                            |
