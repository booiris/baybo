# config - Unified Configuration Loading and Validation

## Overview

The `config` crate owns the root `BayboConfig` struct, JSON loading, and the `validate()` method. It centralizes settings that were previously scattered across individual crates or hardcoded in `main.rs` (context-window budget, channel buffer sizes, rate limits, etc.).

A single JSON file — typically `baybo.json` — maps 1:1 to `BayboConfig`. Consumers (the `baybo` bin crate's boot/runtime layer, plus `baybo-cli`, `baybo-gateway`, `baybo-memory`, and `baybo-setup`) map each section into the corresponding domain type.

Top-level entries: `llm` (a `Vec<LlmEntry>`) plus `default-llm: LlmEntryName`, `agent`, `channels`, `security`, `skills`, `cost`, `workspace`, `gateway`, `browser`, `external_agents`, `memory`, `permission`, and an optional `proxy`.

> **Proxy.** `proxy` is an optional `{ url, no_proxy? }` block (omitted ⇒ direct
> connections). When set, every outbound HTTP call — LLM providers, model
> discovery, OpenRouter pricing, ChatGPT-subscription + OAuth, HTTP MCP,
> WebFetch, CLI `mcp probe` — routes through `url` (`http`/`https`/`socks5`,
> with optional inline `user:pass@` creds), and the standard `*_PROXY` env vars
> are injected into spawned children (bun channel sidecars, node MCP/browser
> stdio servers, external-agent CLIs). Loopback (`localhost`/`127.0.0.1`/`::1`)
> is always kept direct so the local gateway and loopback MCP/CDP endpoints keep
> working; add more bypass entries via `no_proxy`. The boot layer maps
> `ProxyConfig` into the runtime `baybo_security::http::ProxySettings`. `proxy` is
> **not** hot-reloadable — changing it rejects the reload (restart required).

> **MCP status note.** MCP server records do **not** live in `baybo.json`.
> They live in `<workspace>/config/.mcp.json`, owned by `baybo-tools::mcp`
> (config shape: `McpFile`, `McpServerEntry`, `McpTransportConfig`,
> `OAuthConfig`, plus its own `TrustLevelConfig`). Per-tool execution
> timeouts are declared by each tool itself via `Tool::max_timeout`
> (defaults to 30 s). See `docs/modules/tools.md` for the MCP client
> architecture and per-tool timeout overrides, and `docs/modules/cli.md`
> for the `baybo mcp {add,list,get,remove}` surface.

There is no `storage` section. Storage paths are **derived** from the project root (`workspace.path`) — operators choose a project root, not individual data-file locations.

## Design Decisions

### Leaf-level placement in the dependency graph

The crate sits near the leaf of the dependency graph: its only runtime `baybo-*` deps are `baybo-model` (for shared newtypes the config surface reuses directly — `LlmEntryName`, `ModelTier`, `MicroUsd`, `ExternalAgentKind`) and `baybo-workspace` (paths only, pulled in with `default-features = false` to keep the I/O layer out of the dep graph). It otherwise depends on external libraries only — `serde`, `serde_json`, `tokio`, `thiserror`, `parking_lot`. This keeps the surface small and deliberate:

- Avoids coupling the config surface to most domain type changes
- Keeps `config` cheap to build, low in the graph
- Prevents circular dependencies when `agent` wants to read configuration

To compensate, `config` defines **mirror structs** for domain types it references (e.g., `TrustLevelConfig` in `baybo-config::tools` mirrors `baybo_model::TrustLevel`). Mapping between mirror and domain types happens at the consumer (startup code or `agent` bootstrap). See §"Mirror maintenance contract" for drift prevention. The MCP-specific mirrors (`McpServerEntry`, `McpTransportConfig`, `OAuthConfig`, plus a second `TrustLevelConfig`) live in `baybo-tools::mcp::config` because MCP server records are persisted in `<workspace>/config/.mcp.json` rather than `baybo.json`.

### Defaults-first serde strategy (top-level only)

Every **top-level** entry carries `#[serde(default)]` or is covered by the root default, with a matching `Default` impl. An empty JSON object `{}` deserializes into a fully valid `BayboConfig`; users only specify fields they want to override.

This does **not** extend uniformly into nested structs. The following nested types have required serde fields — supplying the parent object without them fails at deserialization, not in `validate()`:

- `TelegramChannelConfig` (`enabled`, `bot_token_env`) — under `channels.telegram`
- `DiscordChannelConfig` (`enabled`, `bot_token_env`) — under `channels.discord`
- `WeixinChannelConfig` (`enabled`) — under `channels.weixin`
- `ProxyConfig` (`url`) — the top-level `proxy` block
- `LlmEntry` (`name`, `provider`, `model`) — every element of the top-level `llm` array

Required-ness beyond serde (non-empty strings, numeric ranges, URL schemes) is enforced by `validate()`.

### Collect-all validation, not fail-fast

`BayboConfig::validate()` walks every section and accumulates all `ValidationError` entries before returning. The returned `ConfigError::Validation(Vec<ValidationError>)` surfaces every problem at once so users can fix the entire file in one pass rather than iterating on single errors. `BayboConfig::load_from_str` and `load_from_file` call `validate()` internally — callers do not need to invoke it separately.

### JSON format (not TOML or YAML)

JSON is the sole supported format. It has the widest tooling support, round-trips through `serde_json`, and matches the project's existing use of JSON for trace payloads.

### Unknown fields

`serde`'s default tolerance applies: unknown keys are silently ignored. This is permissive by design so a newer JSON file (with fields an older binary does not yet know about) does not hard-fail at load. The cost is that typos in field names are also silent — `"agent": { "max_iteration": 10 }` parses fine and the real `max_iterations` stays at its default.

Sections that must not accept typos (security-sensitive or governance-sensitive shapes, e.g. `security`) may opt into `#[serde(deny_unknown_fields)]` individually; today no section has. The root `BayboConfig` intentionally keeps permissive semantics.

### LLM entries and `model_list`

An `LlmEntry` splits into two halves that behave differently:

- **The entry** owns what its models genuinely share: `provider`, `base_url`, `api_key_env`, and `reasoning_effort`. Effort sits here because it is a *preference*, not a fact about a model — a session's own thinking-level pick (`sessions.last_effort`) overrides it per request, and the provider clamps it to what the chosen model allows.
- **`model_list`** owns per-model facts: `context_window`, `pricing`, `supports_vision`. Every item is an object `{model, context_window?, pricing?, supports_vision?}` — one shape, whether or not the model carries overrides.

`model` names the entry's default. `LlmEntry::models()` normalizes with one rule — **prepend `model` when `model_list` doesn't already contain it** — so listing only the *extra* models is equivalent to listing the default first, and both resolve to `[default, …rest]`. An operator who lists the default explicitly keeps control of its position, and that order is the chat model picker's order.

Effective value = the model's own spec field → the provider factory's per-model resolution (bundled OpenRouter snapshot, keyed by model slug) → a per-provider constant. An unset override is therefore already per-model-correct; the spec only exists for the cases the snapshot gets wrong.

`lite_model` names one of the entry's models — the cheaper one used for the agent's **auxiliary** LLM calls. It is validated against `models()` in `validate()`, not at client-build time, so a stranded reference is rejected by a reload dry-run without constructing anything, and no separate client is ever built for it (it is by definition one of the entry's own models).

Resolution, most specific first (`LlmClientPool::resolve_lite`):

1. the resolved entry's own `lite_model` — same provider, same credentials, so nothing the user typed changes hands;
2. otherwise `agent.model_tiers.lite`, that entry's **default** model — no second hop into *its* `lite_model`;
3. otherwise the session's own client, i.e. the pre-`lite_model` behaviour.

Step 3 is not optional. The Bash risk judges are fail-closed, so a "no lite configured" answer of `None` would silently turn the default `permission = auto` from "judge every destructive command" into "prompt on every destructive command".

Which calls are auxiliary is decided by one rule: **an auxiliary call may use the lite model only if its input is not the session transcript.** The Bash risk judges (a command line), WebFetch's page summary (a fetched page), and title generation (one user message) qualify. Context compression and the progress observer do not — their input is the exact prefix provider prompt-caching keeps warm, and that cache is per-model, so moving them would trade a cache hit for a cold full-transcript read and can cost *more* than the model it saves.

`baybo setup` / `baybo llm add` seed `lite_model` to the entry's **own** model. That is a behavioural no-op — resolution step 1 then hands back the entry's default client, exactly as an unset field would — written purely so the knob is visible: the field is `skip_serializing_if = "Option::is_none"`, so leaving it unset means an operator has no way to learn from their own config file that the auxiliary calls can be moved to a cheaper model. Editing it to a cheaper entry model is the intended next step.

A `lite_model` is never added to the entry's pinnable set: it is what the runtime picks for itself, not a menu item. An operator who wants it in the chat picker lists it in `model_list` as well, and it is then built once and serves both roles.

The entry-level `context_window` / `pricing` / `supports_vision` keys that predate `model_list` are simply **gone** — no tombstone field, no migration check. A config still carrying them parses, and the keys are ignored like any other unknown key (§"Unknown fields"); the affected models fall back to the factory's per-model resolution. This follows the repo rule against legacy-data migrations: remove the field and its consumers, leave the orphaned data inert.

The `PUT /v1/llm/models/{name}` admin endpoint and `baybo llm edit` both address the **default** model's spec when they set one of the three (materialising it at the front of `model_list` if needed). Overrides for an entry's other models are config-file edits only.

### Secret handling

Config does **not** store live secret values; it stores references:

- `LlmEntry::api_key_env` is a reference to an env-var name (e.g., `"OPENAI_API_KEY"`), not raw key material. `llm.md` §Constraints prohibits inline keys. When absent, `baybo_llm::credentials::resolve_api_key` falls back to the per-entry vault key (`llm.entry.<name>.api_key`) and then to provider-specific defaults (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `MINIMAX_API_KEY`).
- `SecurityConfig::encryption_key_file` is the only encryption-key source: an absolute path to a hex-encoded 32-byte file (mode 0600). `baybo setup` mints one at `<workspace>/.key/encryption.key`. A missing or unreadable file is a hard error at startup — there is no env-var alternative and no dev-key fallback.

### Section boundaries

Sections mirror Baybo's real runtime concerns, not a 1:1 copy of any external reference:

| Section    | Maps to                                                     | Notes                                                                                                                                                                                                |
| ---------- | ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `llm`      | `Vec<LlmEntry>` + `default-llm: LlmEntryName` → `baybo_llm::LlmProviderConfig` | Each `LlmEntry` is a `{name, provider, model, model_list?, lite_model?, api_key_env?, base_url?, reasoning_effort?}` record — provider + credentials on the entry, per-model facts in `model_list`. See §"LLM entries and `model_list`". `default-llm` names the entry the agent loop uses by default; the field is serde-renamed to `default-llm`. Multiple entries can target the same provider with distinct credentials. |
| `agent`    | `AgentLoopConfig` (`max_iterations`) + `ContextManager`/`TokenBudget` (context budget, `keep_recent`) + subagent caps + tier map | Carries `max_iterations`, the context-window budget, `max_subagent_depth`, `max_subagents_per_root`, and `model_tiers` (`ModelTier` → `LlmEntryName`; keys are `lite`/`balanced`/`deep`, with `fast` accepted as the pre-rename spelling of `lite`). **`model_tiers.lite` does double duty** — it is both `spawn_subagent`'s cheap tier and the fallback for auxiliary LLM calls, so re-pointing it moves both. An operator who wants them apart sets a per-entry `lite_model`, which outranks it. Per-tool timeouts are not configured here — each `Tool` impl declares its own ceiling via `Tool::max_timeout` (default 30 s). |
| `channels` | `ChannelRegistry` adapter enablement + mpsc buffer sizes    | See §"Channel enablement model".                                                                                                                                                                     |
| `security` | `EncryptionKey` location + `LeakDetector` enablement        |                                                                                                                                                                                                      |
| `skills`   | `baybo_skills_assessor::AssessmentMode`                      | `risk_check`: `off` disables the LLM classifier, `primary` (default) judges `SKILL.md` only, `full` judges the whole directory tree.                                                                 |
| `cost`     | `SpendingLimits` + `baybo_agent::router::LiveRateLimit` (via `RouterConfig.rate_limit`; hot-swapped on reload) |                                                                                                                                                                            |
| `workspace`| `WorkspacePaths` + storage path composition                 | Single field: `path`. The project root from which all persistent data paths are composed (e.g. `<workspace.path>/state/storage.db`).                                                                |
| `gateway`  | `baybo_gateway::RuntimeGatewayConfig`                        | Admin bind address + port, CORS allowlist, shutdown grace. See [`gateway.md`](gateway.md).                                                                                                          |
| `browser`  | `baybo_tools::browser` configuration                         | Browser sidecar launch settings (docker mode, profile path).                                                                                                                                         |
| `external_agents` | `baybo_agent::external_agent` registry                | Per-kind switch for the host-execution external agents — `claude`, `codex` (each `{ enabled, binary_path? }`). `enabled` defaults to **`true`**: boot probes `PATH` and registers whichever binary is actually installed, so having the CLI on the host is the opt-in. Set `false` to withhold an installed backend — worth knowing that these run their own tool loops with approvals bypassed. `binary_path` records the resolved absolute path `setup` probed, so the gateway (different cwd, narrower `PATH`) pins the same binary. |
| `memory`   | `baybo-memory` backend selection                             | `{ enabled (default false), provider: noop\|mem0\|openviking, llm?, extra? }` — `llm` names the entry used for salience/extraction (unset ⇒ `default-llm`); `extra` is an opaque per-plugin bag (a documented exception to the typed-over-`Value` rule). Not hot-reloadable. See [`memory.md`](memory.md). |
| `permission` | `baybo_tools::builtin::BashPermissionMode`                   | Shell-out permission policy: `auto` (default), `manual`, or `free` (`open`/`none` accepted as legacy aliases for `free`). Hot-reloadable through `LivePermissionMode`; see [`../permission.md`](../permission.md). |

`registry` and `cron` currently have no top-level section. See §"Out-of-scope modules" for rationale and planned placement.

### Channel enablement model

Each optional channel (`telegram`, `discord`, `weixin`) is wrapped in `Option<_>`: **absent ⇒ disabled, present ⇒ enabled**. The inner `enabled: bool` is redundant with the `Option` wrapper and is retained only for migration. `validate()` enforces this self-consistency for the two token-backed channels — `validate_channels` rejects `Some { enabled: false, ... }` for `telegram` and `discord` and guides the operator to omit the section instead. `TelegramChannelConfig` and `DiscordChannelConfig` each carry an additional required `bot_token_env`; `WeixinChannelConfig` has only `enabled: bool` (no token field) and is not currently inspected by `validate()`. The `cli` channel is always present because it has no required configuration and ships as the default adapter. The `owner` channel is likewise always installed — it powers the embedded web dashboard / chat page and the paired mobile app (both register as `owner`), has no operator-facing knobs, and so carries no `channels` section of its own.

## Out-of-scope modules

The following modules do not (yet) have sections in the root config. This is a deliberate phased decision, not an oversight. Each has a planned placement:

- **registry** — artifact source allowlist, signature verification policy, trust ceilings. Today the defaults are baked into the registry constructors.
- **cron** — scheduler poll interval, max concurrent runs, missed-run policy. Today `CronScheduler` uses compile-time defaults.

Principle: a module earns a config section when operators need to tune it in production. Until that need appears, keeping the surface small avoids defaults sprawl.

## Mirror maintenance contract

`baybo-config` holds mirrors of selected domain types (today, `TrustLevelConfig` in `baybo-config::tools` mirrors `baybo_model::TrustLevel`). The MCP-specific mirrors (`McpServerEntry`, `McpTransportConfig`, `OAuthConfig`, plus a second `TrustLevelConfig`) live in `baybo-tools::mcp::config`, not here, because `.mcp.json` is owned by `baybo-tools`. Drift prevention applies to both crates:

1. **Ownership** — mirrors live in `baybo-config`. Whenever the upstream domain type (e.g. `baybo_model::TrustLevel`) changes shape, the same PR updates the mirror and the conversion between them.
2. **Contract tests** — each mirror has a round-trip test in `baybo-config`'s integration tests, converting domain → mirror → domain over every variant. These act as the drift detector: adding a variant upstream without a mirror update breaks match exhaustiveness and fails CI.
3. **Forward compatibility** — domain enums that mirrors target should be `#[non_exhaustive]`; the mirror→domain conversion returns a typed `ConfigError::UnsupportedVariant { ty, variant }` rather than panicking when it encounters an unknown variant.
4. **Scope limit** — only types that appear in the config surface are mirrored. Transient/internal domain types must not leak into `baybo-config`.

## Reload semantics

`baybo-config` ships the reload **primitives**, not the orchestration. As a leaf crate it owns two pure pieces in `reload.rs`: a live, swappable handle to the applied config (`ConfigHandle`) and the whitelist gate (`hot_reload_diff`). The fallible derived-state rebuilds (the LLM pool, cost limits) and the end-to-end reload flow live in consumer crates — see [`docs/config-hot-reload.md`](../config-hot-reload.md) before touching reload code. The contract below is the part `baybo-config` itself enforces.

- **Live handle** — `ConfigHandle` wraps `Arc<parking_lot::RwLock<Arc<BayboConfig>>>`. `current()` clones out the applied `Arc`; `store()` is the infallible commit half that swaps a new `Arc` in. Reads happen per-turn / per-request (resolving the active model, dashboard reads), never per-token, so a plain `RwLock<Arc<_>>` is ample — no `ArcSwap` dependency. The previous `Arc` stays alive until its last in-flight reader drops it, which gives the "in-flight requests finish on the old config" behaviour below.
- **Hot-updatable whitelist** — `hot_reload_diff(old, new)` enforces an explicit allowlist: `llm`, `default_llm`, `agent.model_tiers`, `cost.rate_limit`, `cost.spending_limits`, and `permission`. Any reload whose diff touches a field **outside** this set hard-rejects the entire reload (atomic — nothing swaps) with `ConfigError::NotHotReloadable { section }` naming the offending section. Not hot-updatable: `gateway.*`, `workspace.path`, `security.*`, `channels.*`, `skills.*`, `browser.*`, `external_agents.*`, `proxy`, `memory.*`, and the rest of `agent` (`max_iterations`, `context`, `max_subagent_depth`, `max_subagents_per_root`). `new` is destructured field-by-field so adding a field to `BayboConfig` or `AgentConfig` forces a hot/non-hot classification here rather than silently defaulting to "hot, unchecked".
- **Atomic swap** — a successful reload swaps a single `Arc<BayboConfig>` holding all whitelisted changes together. Partial application is forbidden.
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
| `llm[i].model_list[j].model`          | non-empty                                            |
| `llm[i].model_list[j].context_window` | if set, > 0 (a zero window collapses the compression threshold and the WebFetch summariser budget) |
| `llm[i].lite_model`                   | if set, non-empty **and** names one of the entry's `models()` (default + `model_list`) |
| `llm[i].base_url`                     | if set, scheme is `http://` or `https://`            |
| `llm[i].api_key_env`                  | if set, valid env-var identifier                     |
| `default-llm`                         | when `llm` is non-empty, must name an existing entry |
| `agent.max_iterations`                | in `1..=1000`                                        |
| `agent.context.compression_threshold` | in `(0.0, 1.0]`, finite                              |
| `agent.context.keep_recent`           | ≥ 1                                                  |
| `agent.max_subagent_depth`            | ≤ 32                                                 |
| `agent.max_subagents_per_root`        | in `1..=256`                                         |
| `workspace.path`                      | non-empty; absolute (no `./`, no `~`)                |
| `browser.chrome_path`                 | if set, absolute                                     |
| `browser.profile_dir`                 | if set, absolute                                     |
| `channels.message_buffer_size`        | in `1..=65536`                                       |
| `channels.telegram` / `channels.discord` | `enabled: false` is rejected — omit the section instead (enablement-model self-consistency) |
| `channels.telegram.bot_token_env`     | non-empty                                            |
| `channels.discord.bot_token_env`      | non-empty                                            |
| `cost.spending_limits.daily_usd`      | if set, strictly positive, finite                    |
| `cost.spending_limits.monthly_usd`    | if set, strictly positive, finite                    |
| `cost.spending_limits`                | cross-field: `daily_usd ≤ monthly_usd` when both set |
| `cost.rate_limit.*`                   | `max_requests ≥ 1`, `window_secs ≥ 1`                |
| `gateway.bind_address`                | non-empty                                            |
| `gateway.port`                        | > 0                                                  |
| `gateway.shutdown_grace_secs`         | ≥ 1                                                  |
| `gateway.cors_allowed_origins[i]`     | non-empty                                            |
| `proxy.url`                           | when `proxy` is set: non-empty; scheme one of `http`/`https`/`socks5`/`socks5h`/`socks4`/`socks4a` |

### Cross-section rules

Field-level checks catch syntax errors; cross-section checks catch policy inconsistencies. These live in a dedicated `validate_cross_section(config, errors)` pass that runs after the per-section passes complete:

| Rule                                                                                                                                                                             | Sections involved  |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------ |
| `default-llm` (when `llm` is non-empty) must reference an existing `llm[i].name`                                                                                                 | `llm`              |
| Each `agent.model_tiers` target must name an existing `llm[i].name`                                                                                                             | `agent` / `llm`    |
| `security.encryption_key_file` must be set and absolute (string path to a hex-encoded 32-byte file; no `./`, no `~`)                                                             | `security`         |

The MCP-specific trust/capability rules (stdio requires `trusted`, `installed`/`untrusted` may not declare `WriteFile`/`ExecCommand`) live with the MCP file in `baybo-tools::mcp::config` since `.mcp.json` is owned there.

Cross-section rules are part of the default `validate()` pass. A future strict-load flag will also enforce advisory hygiene (e.g. key-file extension hints, env-var name syntax); today those are handled case-by-case in bootstrap.

## Constraints

- Near-leaf in the dependency graph: the only runtime `baybo-*` deps are `baybo-model` (shared newtypes the config reuses) and `baybo-workspace` (paths only). No dependency on the heavier domain crates (`agent`, `llm`, `tools`, …)
- No secret plaintext in the config struct; only references (env var names, file paths)
- Validation must be pure — no I/O, no time-of-day dependencies, no filesystem probes
- All top-level sections must provide a `Default` impl whose output passes `validate()`
- Mirrors of domain types must satisfy §"Mirror maintenance contract"
- Runtime config mutations go through the §"Reload semantics" primitives (`ConfigHandle` + `hot_reload_diff`); only the whitelisted sections may change live, and the orchestration lives in consumer crates

## Collaboration

| Module     | Role                                                                                                                                                                                   |
| ---------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `main.rs`  | Loads the config at startup, maps each section into domain types, passes them down                                                                                                     |
| `agent`    | Receives domain values the bin crate builds from the `agent` / `cost` / `external_agents` sections (`AgentLoopConfig`, `SpendingLimits` / `LiveRateLimit`, `build_registry` entries) — no direct `baybo-config` dep |
| `llm`      | Receives `LlmProviderConfig` built from each `LlmEntry`                                                                                                                                |
| `tools`    | No `baybo.json` section: per-tool timeouts come from `Tool::max_timeout`. (MCP server records live in the workspace-local `<workspace>/config/.mcp.json`, owned by `baybo-tools::mcp` — see the MCP status note above.)                                 |
| `channels` | Channel adapters are registered based on `ChannelsConfig` section enablement                                                                                                           |
| `trace`    | `LlmCall` spans carry a `provider_config_hash` on their begin payload (`LlmCallBegin`, `baybo-trace::span`) — the slot for which provider config a call ran under; today every call site writes it empty and the hash is not yet computed. Config load/change itself is not traced. |
