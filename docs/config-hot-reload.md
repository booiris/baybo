# Config Hot Reload

**Status:** implemented (branch `config-hot-reload`).
This is the approved design for config hot-reload. It folds together what were originally two `docs/todo/` notes (the general reload framework and the LLM-identity carve-out, since removed): the framework lands first, and LLM identity + cost limits are its first two consumers.

## Goal

Apply a subset of `aura.json` changes to a running gateway without a restart, honoring the contract in [`docs/modules/config.md`](modules/config.md) §"Reload semantics": an explicit hot-updatable whitelist, an atomic swap, validation rollback, and in-flight isolation.

The headline win is **LLM identity** (`provider`, `model`, `base_url`, `api_key`, `pricing`, `context_window`, `default-llm`, `model_tiers`) — today a restart, full stop — plus **cost limits** (`cost.rate_limit`, `cost.spending_limits`).

## Non-goals

- Hot-reloading anything outside the whitelist (ports, bind address, workspace path, encryption key file, channels, session, the rest of `agent`). These **hard-reject** on reload.
- HTTP add/remove model endpoints — the `aura llm` CLI already does full CRUD, and reload rebuilds the whole pool from `config.llm` regardless of which surface triggered it.
- TUI inline reload (the TUI boot path has no admin HTTP server; SIGHUP only, if wired).
- Making `skill_assessor` *follow* a hot-reload model swap — it's now on the billed path (a system-attributed `BilledLlm`) but stays pinned to the boot-time default; see the TODO below.

## Contract (non-negotiable)

Restated from `config.md` §"Reload semantics", made concrete for this work:

- **Hot-updatable whitelist:** `llm`, `default_llm`, `agent.model_tiers`, `cost.rate_limit`, `cost.spending_limits`. Any diff touching a field outside this set rejects the **entire** reload (atomic — nothing swaps) with an error naming the offending section. An operator who edits a model *and* a port in one shot gets the model change rejected too and must restart; this is deliberate and predictable.
- **Atomic swap:** a successful reload swaps a single `Arc<AuraConfig>` holding all whitelisted changes together. Partial application is forbidden. *(Implementation note: commit publishes in three sub-swaps — pool, then cost limits + rate atomics, then the config handle — not one compare-and-swap. Each value is individually valid, so there's no torn read, but a turn starting in the ~ns gap can observe new-pool + old-limits. Practically fine; not a literal single publish.)*
- **Validation rollback:** a reload that fails `validate()` leaves the running config untouched and returns `ConfigError`; no observable partial state.
- **In-flight behavior:** an LLM turn already running finishes on the client it resolved at turn start; only the next turn sees the new pool.

## Architecture

### Layering

`aura-config` is a leaf crate (no `aura-*` deps), so it owns only the pure machinery:

- `ConfigHandle` — newtype over `Arc<parking_lot::RwLock<Arc<AuraConfig>>>` (parking_lot is already the workspace-wide lock primitive; the swap is read per-turn / per-request, not per-token, so lock-free `ArcSwap` buys nothing and would add a new dep to ~5 crates). `current()` clones out the live `Arc`; `store()` swaps under the write lock. Holds the **current applied** config; the orchestrator diffs against it and stores the new one on a successful commit.
- The orchestrator re-reads through the existing `AuraConfig::load_from_file` (already a load+validate wrapper); no separate `reload_from_file` is needed.
- `hot_reload_diff(old: &AuraConfig, new: &AuraConfig) -> Result<(), ConfigError>` — pure; returns `Err` naming the first non-whitelisted section that changed. No I/O, matching the crate's "validation must be pure" constraint.

The heavy lifting can't live in `aura-config` (it would need `aura-agent`/`aura-cost`/`aura-llm`). It lives behind a trait:

- **`trait ConfigReloader`** (defined in `aura-gateway`, so `AdminState`/`GatewayDeps` can name it): `async fn reload(&self) -> Result<ReloadOutcome, ReloadError>`.
- **Concrete impl in the bin crate** (`src/`), because it calls `boot::build_llm_client_for_entry`. It owns the `ConfigHandle`, a concrete `LlmReloader`, a concrete `CostReloader`, and a **reload `Mutex`** so concurrent triggers (endpoint + SIGHUP racing) serialize. Handed to `GatewayDeps` as `Arc<dyn ConfigReloader>`; the SIGHUP handler holds the same `Arc`.

`ReloadOutcome` summarizes what changed (new active model, entries added / removed / dropped-on-build-failure) for the HTTP response + logs.

### Two-phase reload flow

Driven under the reload `Mutex`:

1. `new = AuraConfig::load_from_file(path)?` — load + validate. Fail ⇒ return, nothing swaps.
2. `old = handle.load()`.
3. `hot_reload_diff(&old, &new)?` — non-whitelisted change ⇒ reject, nothing swaps.
4. **Prepare** (fallible):
   - `LlmReloader::prepare(&old, &new)` rebuilds the pool concurrently from `new.llm` (same `futures::join_all` shape as boot). **Default-entry build failure ⇒ abort the whole reload.** Non-default failure ⇒ drop that entry with a `warn!` (mirrors boot's policy exactly). Also computes the new pricing overlay (`id → pricing` from the rebuilt clients' `model_info`) and the new refresh `(provider, model)` pairs.
     - **No separate cheap path** (deviation from the original step 4): client construction is cheap — a struct build plus a vault read, with no network round-trip at construction — so `prepare` always rebuilds the whole pool and `commit` re-seeds pricing from the rebuilt clients. This keeps each client's `model_info` (pricing, context window, capabilities) consistent with the edit and avoids replicating the snapshot+override pricing merge outside the factory. A pricing-only edit therefore costs one cheap pool rebuild rather than a bespoke fast path.
   - `CostReloader::prepare(&old, &new)` is infallible — just captures the new `SpendingLimits` and rate-limit params.
5. **Commit** (infallible), ordered to avoid a window where a new model bills at $0:
   1. `cost_manager.merge_pricings(new_overlay)` — **first**, so pricing for any new model id is in the map before the pool can serve it.
   2. swap the pool handle (`*pool_handle.write() = new_pool`).
   3. `config_handle.store(new)`.
   4. `cost_manager.set_limits(new_limits)` + write the new rate-limit atomics.
   5. cancel the old refresh task's `CancellationToken` and respawn it with the new pairs (immediate first fetch).

Only `LlmReloader::prepare` can abort a reload after validation; everything in commit is a swap or a setter that cannot fail.

Every reload runs the full set of steps — the LLM pool is **always** rebuilt, never gated on the config diff. A vault credential rotation changes nothing in `config.llm` (the key lives in the vault), so a diff-gated rebuild would leave a rotated key serving the old credential; and there's no cheap way to prove "credentials unchanged". The rebuild is local/cheap and the per-turn `Arc::ptr_eq` rebind it triggers is prompt-cache-safe (a fresh client for an unchanged model still hits the provider's prefix cache), so an unconditional rebuild is the simple, correct choice. The follow-up "incremental rebuild" (reuse unchanged entries' client `Arc`s) would trim the rebind churn without reintroducing the credential gap — see "Out of scope".

## LLM consumer

### Pool handle + per-turn swap

`LlmClientPool` is held as `Arc<parking_lot::RwLock<Arc<LlmClientPool>>>` (alias `LlmPoolHandle`). This changes the `AgentLoopConfig.llm_pool` field type, which ripples to every construction site (the single `wire_router` spawn closure in `src/runtime.rs` — used for top-level, cron, subagent, and background-compression actors alike — plus the integration-test harness and `AgentLoop` unit tests).

`AgentLoop` resolves at **turn start** (not construction): `pool_handle.read().resolve(self.initial_llm)`, pinned for the whole turn (a turn may issue many LLM calls; they all use one model). When the resolved client differs from the current one **by pointer** (`Arc::ptr_eq`, *not* by model id — a reload always builds fresh `Arc<BillableLlm>`s, so this also catches a `base_url` / credential / `reasoning_effort` / `context_window` edit that kept the same model id; an unchanged pool returns the same `Arc`, so the common path stays a no-op), the loop:

- swaps `self.llm_client`,
- rebuilds `self.billed_chat_factory` from the new client (so in-tool side-LLM calls bill the new model),
- calls `context_manager.set_active_model_context_window(new)` — load-bearing: a smaller replacement context would otherwise overflow because compression still gated on the old larger window.

The **tokenizer is deliberately not swapped**. `TiktokenTokenizer` is already an estimate and the `TokenCalibration` layer corrects drift against observed usage within a few turns; adding a mutable tokenizer surface to `ContextManager` isn't worth it.

Subagent pinning needs no special handling: `resolve(Some(removed_entry))` already falls back to the default with a `warn!`, and a pinned entry whose model changed picks up the new model on its next turn.

### Admin read-through (no stale snapshots)

`AdminState` holds the pool handle instead of a captured `Arc<BillableLlm>`; `get_llm` resolves the current default per request, so it's always truthful after a reload. (`aura-gateway` already depends on `aura-agent`, so it can name the pool type directly — no trait indirection.)

The generic config endpoints (`GET`/`PUT`/`DELETE /v1/config`) read the **current on-disk** config via `read_config_for_dashboard`, not the boot `state.config` snapshot. Otherwise `GET` would lie after a reload, and `PUT`/`DELETE` would build from the stale snapshot and write it back — clobbering changes a prior hot-reload already applied. (The LLM admin endpoints already used `read_config_for_dashboard`.) The path `read_config_for_dashboard` reads and the path the reloader applies are the **same** value (`resolve_config_path()`, or the default file when none existed at boot) — otherwise a first-run create + reload would apply the file to the live pool yet leave `GET /v1/config` still reporting the empty boot snapshot.

### Pricing re-seed (mandatory)

The cost lookup keys by the active client's `model_info.id` (`crates/agent/src/runtime/billed_chat.rs`); a model id absent from the pricing map attributes **$0**, silently disabling the budget gate for it. So commit's `merge_pricings` from the rebuilt clients is what makes the swap safe — it's not optional. `merge_pricings` only inserts/overlays, so a removed model's stale pricing lingers harmlessly (its id is never looked up again).

### Refresh loop

The OpenRouter live-pricing loop (`src/runtime.rs`, currently a detached `tokio::spawn` over `fetch_overlay_for` with a 24h `REFRESH_INTERVAL` and no handle) gets a `CancellationToken` owned by `LlmReloader`. Commit cancels the old task and respawns with the new pairs, doing an immediate first fetch so a newly-added model gets a live overlay promptly. (With no configured `(provider, model)` pairs the loop isn't spawned at all — a detached token is returned instead of a task that fetches an empty overlay and then sleeps.) (The budget gate is already correct from the commit-time snapshot re-seed regardless; this only refreshes the live overlay.)

### Credential rotation

An API key lives in the **vault**, keyed by entry name — never in `aura.json`. So rotating a key produces an empty `config.llm` diff. This is exactly why `reload` rebuilds the pool **unconditionally** (above) rather than gating on the diff: a diff-gated rebuild would leave the running pool bound to the *old* credential while reporting success (a revoked key would stay live). One extra piece is needed at the write side:

- **Stage before build.** `update_model` writes the new secret to the vault **before** `dry_run`, because every provider requires the key at `create_client` construction; otherwise the pre-flight (and an entry gaining its first key) would build against the absent/old value and could wrongly reject a valid edit. Once staged, the subsequent `reload` rebuilds against the new key; the CLI's `aura llm edit --api-key` path gets the same effect because its post-write SIGHUP also runs `reload` (which always rebuilds).

## Cost consumer

- **`cost.spending_limits`:** `CostManager.limits` (a plain `SpendingLimits` field read in `check()`) becomes `RwLock<SpendingLimits>` with a `set_limits(&self, SpendingLimits)` setter. `CostReloader` holds the `Arc<CostManager>` and sets it on commit.
- **`cost.rate_limit`:** the Router's `RateLimiter { max_requests, window }` lives by value inside the `Router`, which the run loop owns and the reloader can't reach. Lift those two params into shared atomics (e.g. `Arc<AtomicUsize>` + an `Arc<RwLock<Duration>>`, or one `Arc<LiveRateLimit>`), created before the `Router`, read per `check`, and held by `CostReloader` for write-on-commit.

Both are infallible to prepare.

## Triggers

- **Admin endpoints:** `update_model`, `set_default`, the generic `PUT`/`DELETE /v1/config`, plus a new `POST /v1/config/reload`. Each validates and **dry-runs the rebuild before** writing config to disk — so an edit whose default model can't be built is rejected (400) without dirtying the file, which would otherwise be re-read and silently dropped by a later SIGHUP — then calls `reloader.reload()` **inline**; the HTTP response carries the `ReloadOutcome` (new active model) or the validation/prepare error. `MutateResponse.requires_restart` is `false` on a hot change. The generic `PUT`/`DELETE` can target a non-hot field: that persists to disk and reports `requires_restart: true` (via the expected `NotHotReloadable`, not an error) rather than applying live. `update_model` additionally stages a rotated `api_key` into the vault **before** the dry-run (every provider needs the key at client construction, so the build must resolve the new value) — see "Credential rotation" above.
- **SIGHUP:** a new arm in `install_signal_handler` (alongside SIGINT/SIGTERM → shutdown) re-reads the on-disk file and runs the same orchestrator. Covers hand-edits and is the mechanism the CLI uses; since `reload` always rebuilds the pool, a CLI key rotation (vault-only, invisible in the diff) takes effect through this path too.
- **`aura llm` CLI** (`Add`, `Edit`, `Remove`, `Default` — the four mutating subcommands; `Status`/`Probe`/`LiveModel` are read-only): write config (+ vault — an `Edit` can touch *only* the vault, e.g. a key rotation), then `SIGHUP` the gateway PID from `<workspace>/state/aura.lock` (the singleton records it) — but **only when the lock is held** by another process. A free lock means no gateway is running, so the recorded PID is stale; the CLI clears it under the lock before releasing, so a racing sibling CLI that observes the brief hold as `WouldBlock` can't read a stale PID and `SIGHUP` an unrelated, PID-reused process. Best-effort: if no live gateway holds the lock, print "config written; takes effect on next start". The CLI is a separate process and gets no synchronous reload result — that's why the signal path exists. The reloader's config path falls back to the default config file when none existed at boot, so a first-run `aura llm add` that creates the file and signals us still reloads instead of no-op'ing with `NoConfigPath`.

## Failure semantics

| Situation | Result |
| --- | --- |
| New config fails `validate()` | `ConfigError`; nothing swaps. |
| Non-hot field changed | Live state untouched; the persisted edit needs a restart (`requires_restart: true`). The baseline **does** advance to the on-disk config so the divergence can't block future hot reloads — see "Baseline advances on a non-hot reload" below. |
| Hot LLM edit while a non-hot field is pending-restart on disk | `update_model` / `set_default` report `requires_restart: true` (not 400) via the shared `apply_after_write`; the LLM edit is persisted and applies on restart. |
| **Default** LLM entry fails to build | Abort the whole reload; nothing swaps. The admin endpoints `dry_run` before writing, so this is caught without dirtying the file. |
| Non-default LLM entry fails to build | Drop it with `warn!`; reload proceeds (mirrors boot). |
| `update_model` rotates a key, then `dry_run` rejects | **Known partial application:** the new vault secret is already staged (providers need it at client construction) and is **not** rolled back — the vault exposes no per-key delete. The config file is untouched (dry-run runs before the write), but a later reload will resolve the staged key. Scoped to the credential only. |
| Two reloads race | Serialized by the reload `Mutex`; the second sees the first's result as the new `old`. |
| Turn in flight during commit | Finishes on its turn-start client; next turn re-resolves. Commit is three sub-swaps (pool, cost, handle), each individually valid, so a turn starting mid-commit may read new-pool + old-limits for a few ns — no torn state, but not a single atomic publish (see the Atomic-swap note above). |

### Baseline advances on a non-hot reload

`reload` diffs the on-disk config against `ConfigHandle` — *not* against a frozen boot snapshot. When a reload carries a non-hot change it returns `NotHotReloadable` (live state untouched) **but still stores the new config into the handle** before returning. This is essential: a `PUT /v1/config` on a non-hot field (e.g. `gateway.port`) persists to disk and returns `requires_restart: true`, so the disk now diverges from the running process on that field. If the baseline didn't advance, that divergence would re-trip on *every* subsequent reload — silently blocking all hot reloads until restart, and 400-ing later hot edits the operator never associated with the port change. Advancing the baseline makes each `hot_reload_diff` compare against the previously-processed disk state, i.e. *this edit's delta*, so a non-hot persist no longer poisons later hot reloads. It's safe because nothing reads the handle for live behaviour — it is purely the diff baseline (the boot-time non-hot value the process actually uses lives in the already-built server/managers, not the handle).

## Out of scope / follow-up TODOs

- **`skill_assessor` swap-follow:** billing is **fixed** — it now holds an `Arc<BilledLlm>` bound once to a `system:skill-assessor` `Attribution`, so its calls record to `cost_records` under the system bucket (`crates/skills-assessor/src/{queue,assessor}.rs`). What remains: it's bound to the boot-time default client, so it does **not** follow a hot-reload model swap. Re-binding on swap would need the runtime to re-inject the handle (the assessor would hold a swappable slot, or read the pool handle per call). Deferred because verdicts cache by content hash, not model, and pinning a safety classifier to a known model is arguably preferable — also an open question whether the assessor should have its own model config rather than tracking the chat default.
- **Incremental pool rebuild (trim rebind churn):** `reload` rebuilds the **whole** pool every time, minting fresh client `Arc`s for every entry, so every live session rebinds on its next turn. `build_pool_clients` could instead diff per entry-name (config fields + a stored credential fingerprint) and reuse the unchanged entries' `Arc`s, so only sessions pinned to a genuinely-changed entry rebind — and a pricing-only edit, which doesn't alter the client, would reuse every `Arc`. The credential fingerprint is what keeps this correct (a vault rotation changes the fingerprint even with an identical config entry), so it doesn't reintroduce the gap that forced unconditional rebuild. The churn is cheap (client build is local, no network) and doesn't affect provider-side prompt cache, so this is an optimization, not a correctness fix.
- HTTP add/remove model endpoints (CLI covers CRUD today).
- TUI inline reload.

## Test plan

Implemented:

- `hot_reload_diff` (`aura-config`): accepts whitelisted-only diffs; rejects each non-whitelisted section (table-driven), including mixed hot+non-hot ⇒ reject.
- Cost (`aura-cost`): `CostManager::set_limits` is observed by the next `check`.
- Rate limit (`aura-agent`): the live `LiveRateLimit` knobs take effect on the next request without rebuilding the limiter.
- Gateway (`aura-gateway`): the LLM admin endpoints round-trip with inline reload wired in (`update_model` returns `requires_restart: false` via the test stub reloader); the OpenAPI drift check covers the new `POST /v1/config/reload`.
- Gateway (`aura-gateway`): a `dry_run` that rejects the candidate leaves the on-disk config **byte-identical** — the guarantee that an unbuildable edit never dirties the file (`rejected_dry_run_leaves_config_file_untouched`).
- Credentials (`aura-llm`): `resolve_api_key` reflects a rotated vault key — the property the always-rebuild reload leans on, so an `aura llm edit --api-key` / admin key rotation takes effect on the next reload (`resolve_reflects_a_rotated_vault_key`).

Deferred (the orchestrator is bin-only, so these need bin-crate or end-to-end fixtures):

- Reload happy path through `RuntimeConfigReloader`: pool swaps, `merge_pricings` seeds the new model id, `GET /v1/llm` reports the new default.
- Default-entry build failure ⇒ reload aborts, old pool + old config still live (no partial state).
- Per-turn swap: an actor mid-session resolves the new default on its next turn; an in-flight turn finishes on the old client.

## Related

- [`docs/modules/config.md`](modules/config.md) §"Reload semantics" — the contract this honors; firmed up + linked here.
- `src/runtime.rs` — pool build (`with_tier_map`), boot CostManager seed (`merge_pricings`), the `fetch_overlay_for` refresh loop, the `wire_router` spawn closure. All re-run / re-wired on reload.
- `crates/gateway/src/api/admin/llm.rs` — `update_model` / `set_default`; "restart required" → inline reload.
- `crates/agent/src/runtime/billed_chat.rs` — cost lookup keyed by `model_info.id`, the reason the pricing re-seed is mandatory.
- `crates/agent/src/runtime/llm_pool.rs` — `LlmClientPool::resolve` fallback behavior relied on for pinned-entry removal.
