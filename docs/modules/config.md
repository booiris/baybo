# config - Unified Configuration Loading and Validation

## Overview

The `config` crate owns the root `AuraConfig` struct, JSON loading, and the `validate()` method. It centralizes settings that were previously scattered across individual crates or hardcoded in `main.rs` (session timeout, token budget, channel buffer sizes, rate limits, etc.).

A single JSON file — typically `aura.json` — maps 1:1 to `AuraConfig`. Consumers (`main.rs` and `aura-agent`) map each section into the corresponding domain type.

Top-level sections: `llm`, `agent`, `session`, `channels`, `security`, `skills`, `cost`, `workspace`.

> **MCP status note.** MCP server records do **not** live in `aura.json`.
> They live in `<workspace>/.mcp.json`, owned by `aura-tools::mcp` (config
> shape: `McpFile`, `McpServerEntry`, `McpTransportConfig`, `OAuthConfig`).
> Per-tool execution timeouts are declared by each tool itself via
> `Tool::max_timeout` (defaults to 30 s). See
> `docs/modules/tools.md` for the MCP client architecture and per-tool
> timeout overrides, and `docs/modules/cli.md` for the
> `aura mcp {add,list,get,remove}` surface.

There is no `storage` section. Storage paths are **derived** from the project root (`workspace.path`) — operators choose a project root, not individual data-file locations.

## Current status

`AuraConfig` is implemented and unit-tested, but bootstrap does not yet consume it. `src/main.rs` still builds `LlmClient` directly from environment variables (`AURA_LLM_PROVIDER`, `OPENAI_API_KEY`, …) and hardcodes session timeout, tool timeout, mpsc buffer sizes, context budget, and the dev master key. The remaining wiring work:

- Load `AuraConfig` in `main.rs` and map each section to its domain type.
- Replace `build_llm_client_from_env()` with a `LlmConfig → LlmProviderConfig` mapping (see §"Section boundaries" for `fallback_model` orchestration).
- Route secrets through the config indirection rather than reading env vars ad hoc.

Known surface gaps that should be closed before or alongside the wiring:

- `SecretRequirementConfig.access: String` and `McpServerEntry.capabilities: Vec<String>` should become mirror enums (`SecretAccessConfig` = `ReadOnly | ReadWrite`, `CapabilityConfig` = `ReadWorkspace | WriteWorkspace | Http(..) | SpawnProcess | BrowserAutomation`). Current stringly-typed form violates the project's "prefer strong types over strings" rule and defers validation to bootstrap. (These mirrors are removed alongside MCP; they return with the MCP re-add.)

Until these land, the spec below describes target state; deviations are flagged inline.

## Design Decisions

### Leaf-level placement in the dependency graph

The crate depends on external libraries only — `serde`, `serde_json`, `tokio`, `thiserror`. It does **not** depend on any `aura-*` crate. This is deliberate:

- Avoids coupling the config surface to domain type changes
- Keeps `config` buildable in isolation
- Prevents circular dependencies when `agent` wants to read configuration

To compensate, `config` defines **mirror structs** for domain types it references (e.g., `TrustLevelConfig` mirrors `aura_model::TrustLevel`). Mapping between mirror and domain types happens at the consumer (startup code in `main.rs` or `agent` bootstrap). See §"Mirror maintenance contract" for drift prevention. (MCP-specific mirrors like `McpTransportConfig` were removed with MCP support and will return when it's reintroduced.)

### Defaults-first serde strategy (top-level only)

Every **top-level** section carries `#[serde(default)]` and a matching `Default` impl. An empty JSON object `{}` deserializes into a fully valid `AuraConfig`; users only specify fields they want to override.

This does **not** extend uniformly into nested structs. The following nested types have required serde fields — supplying the parent object without them fails at deserialization, not in `validate()`:

- `HttpChannelConfig` (`enabled`, `bind_address`, `port`) — under `channels.http`
  **Deprecated**: the `channels.http` stub is retained for one release for
  back-compatibility with older configs, but the HTTP surface is now owned by
  the top-level `gateway` section (`aura-gateway` crate; see
  [`gateway.md`](gateway.md)). Setting `channels.http.enabled = true` has no
  runtime effect — use `gateway.enabled` and run `aura gateway start`.
- `TelegramChannelConfig` (`enabled`, `bot_token_env`) — under `channels.telegram`
- `DiscordChannelConfig` (`enabled`, `bot_token_env`) — under `channels.discord`
- (`McpServerEntry` and `SecretRequirementConfig` required-field notes are removed alongside MCP support.)

Required-ness beyond serde (non-empty strings, numeric ranges, URL schemes) is enforced by `validate()`.

### Collect-all validation, not fail-fast

`AuraConfig::validate()` walks every section and accumulates all `ValidationError` entries before returning. The returned `ConfigError::Validation(Vec<ValidationError>)` surfaces every problem at once so users can fix the entire file in one pass rather than iterating on single errors. `AuraConfig::load_from_str` and `load_from_file` call `validate()` internally — callers do not need to invoke it separately.

### JSON format (not TOML or YAML)

JSON is the sole supported format. It has the widest tooling support, round-trips through `serde_json`, and matches the project's existing use of JSON for trace payloads.

### Unknown fields

`serde`'s default tolerance applies: unknown keys are silently ignored. This is permissive by design so a newer JSON file (with fields an older binary does not yet know about) does not hard-fail at load. The cost is that typos in field names are also silent — `"session.timeout_minutes": 10` parses fine and the real `timeout_minutes` stays at its default.

Sections that must not accept typos (security-sensitive or governance-sensitive shapes, e.g. `security` and — once reintroduced — `tools.mcp_servers[]`) may opt into `#[serde(deny_unknown_fields)]` individually. The root `AuraConfig` intentionally keeps permissive semantics.

### Secret handling

Config does **not** store live secret values; it stores references:

- `LlmConfig::api_key_env` is a reference to an env-var name (e.g., `"OPENAI_API_KEY"`), not raw key material. `llm.md` §Constraints prohibits inline keys.
- `SecurityConfig::encryption_key_file` and `encryption_key_env` are filesystem and environment indirections; the key bytes are loaded at startup by `agent::security`.

### Section boundaries

Sections mirror Aura's real runtime concerns, not a 1:1 copy of any external reference:

| Section    | Maps to                                                     | Notes                                                                                                                                                                                                |
| ---------- | ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `llm`      | `aura_llm::LlmProviderConfig`                               | `fallback_model` is an orchestration concern, consumed by `agent` (not by `LlmProviderConfig`). Until wiring lands, the field is carried by `LlmConfig` for forward compatibility.                   |
| `agent`    | `ExecutionPolicy` + `TokenBudget` + `Truncate::keep_recent` | Carries `max_iterations` and the context-window budget. Per-tool timeouts are not configured here — each `Tool` impl declares its own ceiling via `Tool::max_timeout` (default 30 s).                |
| `session`  | `SessionManager` timeout + cleanup cadence                  | `timeout_minutes` sets idle expiry; `cleanup_interval_minutes` sets sweep cadence (`0` disables cleanup).                                                                                            |
| `channels` | `ChannelRegistry` adapter enablement + mpsc buffer sizes    | See §"Channel enablement model".                                                                                                                                                                     |
| `security` | `EncryptionKey` location + `LeakDetector` enablement        |                                                                                                                                                                                                      |
| `skills`   | `aura_skills_assessor::AssessmentMode`                      | `risk_check`: `off` disables the LLM classifier, `primary` (default) judges `SKILL.md` only, `full` judges the whole directory tree.                                                                 |
| `cost`     | `SpendingLimits` + `Router::with_rate_limit`                |                                                                                                                                                                                                      |
| `workspace`| `WorkspaceManager` + storage path composition               | Single field: `path`. The project root from which all persistent data paths are composed (e.g. `<workspace.path>/storage.db`).                                                                      |

`registry` and `cron` currently have no top-level section. See §"Out-of-scope modules" for rationale and planned placement.

### Channel enablement model

Each optional channel (`telegram`, `discord`, `http`) is wrapped in `Option<_>`: **absent ⇒ disabled, present ⇒ enabled**. The inner `enabled: bool` is redundant with the `Option` wrapper and is retained only for migration; `validate()` must reject `Some { enabled: false, ... }` and guide the operator to omit the section instead. The `cli` channel is always present because it has no required configuration and ships as the default adapter.

## Out-of-scope modules

The following modules do not (yet) have sections in the root config. This is a deliberate phased decision, not an oversight. Each has a planned placement:

- **registry** — artifact source allowlist, signature verification policy, trust ceilings. Today the defaults are baked into the registry constructors.
- **cron** — scheduler poll interval, max concurrent runs, missed-run policy. Today `CronScheduler` uses compile-time defaults.

Principle: a module earns a config section when operators need to tune it in production. Until that need appears, keeping the surface small avoids defaults sprawl.

## Mirror maintenance contract

`aura-config` holds mirrors of selected domain types (today just `TrustLevelConfig`; `McpTransportConfig`, `SecretAccessConfig`, and `CapabilityConfig` will return with MCP support) to stay decoupled. Drift prevention:

1. **Ownership** — mirrors live in `aura-config`. Whenever the upstream domain type (e.g. `aura_model::TrustLevel`) changes shape, the same PR updates the mirror and the conversion between them.
2. **Contract tests** — each mirror has a round-trip test (`From<DomainType> for MirrorType` and `TryFrom<MirrorType> for DomainType`) in `aura-config`'s integration tests. These act as the drift detector: adding a variant upstream without a mirror update breaks match exhaustiveness and fails CI.
3. **Forward compatibility** — domain enums that mirrors target should be `#[non_exhaustive]`; the mirror's `TryFrom` returns a typed `ConfigError::UnsupportedVariant { ty, variant }` rather than panicking when it encounters an unknown variant.
4. **Scope limit** — only types that appear in the config surface are mirrored. Transient/internal domain types must not leak into `aura-config`.

## Reload semantics

`aura-config` has **no reload API today**. Configuration is loaded once at startup; live changes require a process restart.

When hot reload is implemented, the following contract must be in place **before** reload code lands:

- **Hot-updatable fields** — an explicit whitelist. Plausible candidates: `cost.rate_limit.*`, `cost.spending_limits.*`, `security.leak_detection_enabled`. Clearly not hot-updatable: `channels.http.port`, `channels.http.bind_address`, anything influencing `llm` client identity.
- **Atomic swap** — a successful reload swaps a single `Arc<AuraConfig>` holding all whitelisted changes together. Partial application is forbidden.
- **Validation rollback** — a reload that fails `validate()` leaves the running config untouched and returns `ConfigError::Validation` to the caller; no partial state is exposed.
- **In-flight behavior** — requests already running against the old config continue with its values; only new requests pick up the new config.

## Validation Rules

### Field-level rules

| Section                               | Rule                                                 |
| ------------------------------------- | ---------------------------------------------------- |
| `llm.provider`                        | non-empty                                            |
| `llm.model`                           | non-empty                                            |
| `llm.base_url`                        | if set, scheme is `http://` or `https://`            |
| `llm.fallback_model`                  | if set, non-empty                                    |
| `agent.max_iterations`                | in `1..=1000`                                        |
| `agent.context.max_tokens`            | ≥ 1                                                  |
| `agent.context.compression_threshold` | in `(0.0, 1.0]`, finite                              |
| `agent.context.keep_recent`           | ≥ 1                                                  |
| `workspace.path`                      | non-empty                                            |
| `session.timeout_minutes`             | ≥ 1                                                  |
| `session.cleanup_interval_minutes`    | no constraint; `0` disables cleanup                  |
| `channels.message_buffer_size`        | in `1..=65536`                                       |
| `channels.http.*`                     | non-empty `bind_address`, non-zero `port`            |
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
| When `llm.provider` is set, at least one secret source is resolvable: `llm.api_key_env` (env-var reference), or a provider-specific fallback env var documented for that provider | `llm`, `security`  |
| Each `channels.*` with `enabled: false` is rejected (enablement-model self-consistency)                                                                                          | `channels`         |
| `security.encryption_key_file` and `encryption_key_env` cannot both be unset when any downstream consumer requires an encryption key                                             | `security`         |

(The two MCP-specific cross-section rules — host-allowlist/loopback and the trust/capability matrix — were removed with MCP support. They'll return with the MCP re-add.)

Cross-section rules are part of the default `validate()` pass. A future strict-load flag will also enforce advisory hygiene (e.g. key-file extension hints, env-var name syntax); today those are handled case-by-case in bootstrap.

## Constraints

- No `aura-*` dependencies — leaf level alongside `model`
- No secret plaintext in the config struct; only references (env var names, file paths)
- Validation must be pure — no I/O, no time-of-day dependencies, no filesystem probes
- All top-level sections must provide a `Default` impl whose output passes `validate()`
- Mirrors of domain types must satisfy §"Mirror maintenance contract"
- Runtime config mutations are not supported today; see §"Reload semantics" for the target contract

## Collaboration

| Module     | Role                                                                                                                                                                                   |
| ---------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `main.rs`  | Loads the config at startup, maps each section into domain types, passes them down                                                                                                     |
| `agent`    | Consumes `AgentConfig`, `SessionConfig`, `CostConfig`                                                                                                                                  |
| `llm`      | Receives `LlmProviderConfig` built from `LlmConfig`                                                                                                                                    |
| `tools`    | No `aura.json` section: per-tool timeouts come from `Tool::max_timeout`. (Once MCP lands again, the workspace-local `.mcp.json` continues to own MCP server records.)                                 |
| `channels` | Channel adapters are registered based on `ChannelsConfig` section enablement                                                                                                           |
| `hook`     | `ConfigChange` is an extension point that _observes_ or _vetoes_ proposed changes. It does **not** emit provenance — provenance is recorded by the bootstrap/agent layer into `trace`. |
| `trace`    | Records `provider_config_hash` / `config_version` in `ExecutionProvenance` when config is loaded or changed                                                                            |
