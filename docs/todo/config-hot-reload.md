# Config Hot Reload

## Problem

`aura-config` has no reload API. Configuration is loaded once at startup; any change requires a process restart. `docs/modules/config.md` §"Reload semantics" spells out the target contract but the code does not implement it.

The `ConfigChange` hook point exists in `aura-hook` for future hot-reload and for one-shot startup provenance. Today it fires exactly once at startup — there is no runtime mutation path.

## Proposed Direction

The contract in `config.md` is non-negotiable and must land **before** any reload code ships:

1. **Hot-updatable whitelist** — explicit allowlist of fields safe to swap live. Plausible: `cost.rate_limit.*`, `cost.spending_limits.*`, `trace.snapshot_interval`, `security.leak_detection_enabled`. Explicitly not hot-updatable: `channels.http.port`, `channels.http.bind_address`, anything that influences `LlmClient` identity.
2. **Atomic swap** — a successful reload swaps a single `Arc<AuraConfig>` holding all whitelisted changes together. Partial application is forbidden.
3. **Validation rollback** — if the new config fails `validate()`, the running config stays untouched and the caller gets `ConfigError::Validation` back. No observable partial state.
4. **In-flight behavior** — requests already running against the old config continue with its values; only new requests pick up the new one.
5. **Provenance** — every successful reload emits a `ConfigChange` hook event carrying the old/new config hashes, and `trace` records the transition in `ExecutionProvenance`.

Implementation sketch:

- Introduce `ConfigHandle = Arc<ArcSwap<AuraConfig>>` (or `RwLock<Arc<_>>`) owned by `main.rs`, passed to consumers that accept live changes.
- Add `AuraConfig::reload_from_file(&Path) -> Result<Self>` as a thin wrapper around the existing load+validate.
- Add a `HotReloadable` trait (or a pair of methods on the handle) that compares old/new configs and rejects when a non-whitelisted field changed.
- Wire up a trigger — SIGHUP, an admin HTTP endpoint, or a control channel — that calls the reload path and surfaces errors to the operator.

First consumer should likely be `cost.rate_limit` since the RateLimiter already exists and its state is small and easy to replace.

## Related

- `docs/modules/config.md` §"Reload semantics" — the contract this work must honor
- `crates/hook/src` — `ConfigChange` hook point (currently startup-only)
- `crates/trace/src` — `ExecutionProvenance` destination for reload records
- `crates/agent/src/cost.rs` — RateLimiter, likely first beneficiary
