# llm-codex-oauth — ChatGPT/Codex OAuth as an LLM provider

## Goal

Let baybo users drive `gpt-5`-class models with their **OpenAI ChatGPT/Codex subscription** instead of an `OPENAI_API_KEY`. The HTTP path is `chatgpt.com/backend-api/codex/responses` (Codex Responses API), the credential is a ChatGPT OAuth bearer minted via PKCE against `auth.openai.com`. This is OpenClaw's "Codex OAuth via PI" route — the equivalent for baybo's own agent runtime, not the heavier "wrap the Codex CLI app-server" route (that's the C track, deferred).

Public-knowledge inputs: `client_id = app_EMoamEEZ73f0CkXaXp7hrann`, issuer `https://auth.openai.com`, scopes `openid profile email offline_access api.connectors.read api.connectors.invoke`, with the Codex CLI's extra parameters (`id_token_add_organizations=true`, `codex_cli_simplified_flow=true`, `originator=codex_cli_rs`). Source: `openai/codex` repo, `codex-rs/login/`.

## Non-goals (this module)

- Native Codex app-server runtime (track C)
- Image generation through the Codex Responses backend (separate module)
- Multi-account / workspace switching UI — single active profile is enough for v1
- Auth-state sharing with a locally installed `codex` CLI's `~/.codex/auth.json` — explicit re-login, not import. Cleaner trust boundary; matches OpenClaw's stance after they dropped that import path.

## Surface

### New provider id: `openai-subscription`

Selected via `baybo.json`:

```json
{
  "llm": [
    {
      "name": "codex",
      "provider": "openai-subscription",
      "model": "gpt-5"
    }
  ],
  "default-llm": "codex"
}
```

Naming rationale: this is the explicit "use your OpenAI subscription" path, distinct from `openai` (API-key, pay-per-token billing). The leading `openai-` keeps it grouped with `openai`/`openai-codex`-style ids in the `baybo llm add` provider picker and `baybo llm status` listings and avoids putting a vendor product brand (`chatgpt`) directly in operator-facing config. Open question — could equally be `chatgpt`, `openai-oauth`, or `openai-codex`; final decision deferred to review.

No `api_key_env` is consulted; tokens come from the vault. `base_url` defaults to `https://chatgpt.com/backend-api`. **Default-deny on the bearer destination**: an override is accepted only if the parsed host suffix is on the allowlist (`chatgpt.com` and its subdomains, `auth.openai.com`). Anything else fails at provider construction with `LlmError::Config` so the misconfiguration surfaces at boot rather than leaking the bearer at first request. To deliberately override (operator owns the TOS and credential-leak risk), set the env var `BAYBO_OPENAI_SUBSCRIPTION_UNSAFE_BASE_URL=1` — env rather than `baybo.json` field on purpose, so flipping a bypass requires an explicit shell action.

### Vault entry: `llm.openai-subscription.tokens`

Single canonical key (single profile per baybo process). Stored as a typed bundle:

```rust
// in baybo-llm/src/providers/openai_subscription/token_bundle.rs
#[derive(Serialize, Deserialize, Clone)]
pub struct OAuthTokenBundle {
    pub access_token: String,       // JWT, sent as Authorization: Bearer
    pub refresh_token: String,      // opaque
    pub id_token: String,           // JWT, parsed for account_id + plan_type
    pub account_id: Option<String>, // ChatGPT-Account-Id header value
    pub expires_at: i64,            // Unix seconds, from JWT exp claim
    pub obtained_at: i64,           // Unix seconds, for diagnostics
}
```

The vault gets a typed accessor pair so callers never see raw bytes:

```rust
// in baybo-security/src/secret_vault.rs (new)
impl SecretVault {
    pub async fn store_typed<T: Serialize>(&self, name: &str, value: &T) -> Result<()>;
    pub async fn get_typed<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>>;
}
```

Existing `store_secret` / `get_secret` (raw bytes) stay for symmetric-key style entries; typed accessors are layered on top with `serde_json` and the same AES-GCM encryption underneath. The bundle type itself stays in `baybo-llm` — `baybo-security` doesn't learn about OAuth.

### CLI surface (folded into `baybo llm`)

OAuth login is not a separate `auth` subcommand; it's wired into the existing entry-management flow. Picking the `openai-subscription` provider triggers an interactive login that mirrors what a dedicated `auth login` would do, and removal handles `revoke + clear` symmetrically.

| Command | What it does |
|---|---|
| `baybo llm add` (pick `openai-subscription` provider) | Prompts the operator to choose between two login methods: **PKCE** (default on a TTY) opens `https://auth.openai.com/oauth/authorize?...` and runs a one-shot HTTP listener on `127.0.0.1:1455` for the callback; **Device code** (auto-selected on non-TTY stdin, also offered on TTY for headless boxes) hits `/api/accounts/deviceauth/usercode` + polled `/deviceauth/token`. The exchanged bundle is persisted in the vault under a single shared key (one profile per workspace). After the login the flow continues with model + reasoning-effort selection. |
| `baybo llm edit` → `OAuth login (re-authenticate)` | Re-runs the same PKCE / device-code dialog and overwrites the vault bundle. Used to recover from a stale refresh token without removing the entry. |
| `baybo llm remove` (entry whose provider is `openai-subscription`) | Deletes the config entry; if no other `openai-subscription` entries remain, also calls `/oauth/revoke` (RFC 7009, best-effort — logs but does not fail the command) and clears the vault entry. **Vault clear failure is logged but not fatal**: the config entry is gone, so the runtime no longer routes through this provider; the next process won't see the token. Server-side revoke success is reflected in the JSON output (`subscription_revoked: true\|false`). |
| `baybo llm status` | Lists every registered entry (name / provider / model / api_key_env). It does **not** surface OAuth token state (expiry, account email, plan type, override state) today — that data is printed once at login time and is otherwise only visible in tracing events. A richer status view is future work. |

Edit/remove/status live in `crates/cli/src/commands/llm.rs`; the `add` flow (shared with the setup wizard) lives in `crates/setup/src/flow/llm.rs`. The OAuth surface itself (`pkce_login`, `device_code_login`, `revoke`, `VaultTokenStore`) is exposed by `baybo_llm::providers::openai_subscription` for the CLI to consume.

## Architecture

### Module: `crates/llm/src/providers/openai_subscription/`

```
openai_subscription/
├── mod.rs                  — module declarations + pub use re-exports only
├── token_bundle.rs         — OAuthTokenBundle, JWT parsing (exp + chatgpt_account_id)
├── token_store.rs          — VaultTokenStore (SecretVault wrapper) + CredentialKey
├── oauth.rs                — pkce_login(), device_code_login(), refresh(), revoke()
├── reasoning.rs            — allowed_efforts_for() (reasoning-effort allow-list)
├── refresh_coordinator.rs  — per-credential token cache, single-flight gate, background loop
├── factory.rs              — OpenAiSubscriptionProviderFactory + base-url validator
└── completion_model.rs     — OpenAiSubscriptionCompletionModel: Codex Responses completion + streaming
```

Module directory and inner struct both use `openai_subscription` / `OpenAiSubscriptionCompletionModel` to match the user-facing provider id; the wire format (Codex Responses API) and impersonation surface (`originator: codex_cli_rs`) are documented in the module's header comment but don't leak into struct names.

Why split: `oauth.rs` and the CLI both need `token_bundle` + `token_store`, but the agent's hot path only needs `completion_model`. Also makes the OAuth bits independently testable.

### `AnyCompletionModel` change

The enum-dispatched `AnyCompletionModel` in `crates/llm/src/lib.rs` (one variant per provider) carries a dedicated variant for this provider:

```rust
OpenAiSubscription(crate::providers::openai_subscription::OpenAiSubscriptionCompletionModel),
```

`OpenAiSubscriptionCompletionModel` exposes inherent `completion()` / `stream()` methods over rig's public request/error types (`CompletionRequest`, `CompletionError`), so it slots into `AnyCompletionModel`'s `completion()` and `stream()` as a uniform match arm alongside the rig-backed providers. We do **not** route through `rig::providers::openai` because:

1. The wire format is the Responses API (`input` / `instructions` / `tools` / `reasoning` / `store=false`), not Chat Completions (`messages`).
2. `Authorization: Bearer <ChatGPT JWT>` plus `ChatGPT-Account-Id: <id>` is required; rig's openai client only knows API-key bearers.
3. We need 401 → refresh-and-retry. rig won't.

### Token store wiring — `baybo-llm` depends on `baybo-security`

`baybo-llm/Cargo.toml` gets `baybo-security = { workspace = true }` added. This is the simplest plumbing path and matches the user-stated decision over the more ceremonial "register codex factory separately" alternative I'd previously considered.

Concrete consequences:

- `LlmProviderConfig` grows an `Option<Arc<SecretVault>>` field. For the openai-subscription factory it MUST be `Some` (returns `LlmError::Config` otherwise); for every other provider it's ignored.
- `with_default_providers()` registers the openai-subscription factory like all the others — boot doesn't have to do anything special.
- `boot::build_llm_client` gains a `vault: Option<Arc<SecretVault>>` parameter (touches every test fixture that builds an `LlmClient` — accepted cost per user).
- The factory's `create()` does **not** read the vault — it just constructs the `VaultTokenStore` and the completion model (sync, no `block_on`). The bundle load is deferred to the async hot path: `ensure_fresh_bundle()` lazily reads the vault on the first request (and re-validates periodically), so a configured-but-not-yet-signed-in provider only errors when a call is actually attempted.

`baybo-llm` already depends on `tokio` for its streams, so the extra `baybo-security` edge doesn't pull in a new ecosystem.

### Refresh policy

Three layers, in order of how often they fire:

- **Background (proactive)**: one tokio task **per credential** (armed by a one-way latch on the `RefreshCoordinator`, not per client) wakes every `BACKGROUND_REFRESH_INTERVAL_SECS = 3600` (1 hour). If the cached bundle is within `BACKGROUND_REFRESH_MARGIN_SECS = 300` (5 min) of expiry, refresh proactively. This keeps a long-idle process from making the next user-facing call pay the refresh latency. The loop does not exit: an empty vault (user logged out) or a permanent refresh error (token revoked) just idles it, so a later re-login resumes proactive refresh without waiting for a new client to be constructed. Exiting instead would need the latch to clear, which reintroduces a window where a credential has zero live loops.
- **Pre-flight (just-in-time)**: if `expires_at < now() + REFRESH_SKEW_SECS (60)` when a request is about to go out, refresh before sending. Safety net for the ~1 hr window the background task could be late on (process just started, system clock skew, background loop hasn't ticked yet).
- **Reactive (401)**: 401 from `/codex/responses` → refresh once, retry once. Second 401 surfaces as a rig `CompletionError::ProviderError("openai-subscription: unauthorized after refresh — ...")` (classified `LlmError::Transient` by `rig_completion_to_error`) — the user must re-login via `baybo llm edit`.
- **Refresh failure**: `refresh_token_expired` / `refresh_token_reused` / `refresh_token_invalidated` → return a typed error and clear the vault entry (so a stale bundle doesn't keep failing). Other 4xx from refresh → transient error, don't clear, surface to caller (or just log + retry-next-tick if the background task hit it).
- **Concurrency**: two nested gates, because the credential outlives any one process.
  - *In-process*: a `tokio::sync::Mutex` around the cached bundle; one in-flight refresh per credential. Cache and gate live on a `RefreshCoordinator` interned in a process-wide table keyed by `CredentialKey { StoreIdentity, vault_key }` (`refresh_coordinator.rs`) — NOT on the client, because one credential backs many clients (the entry's default model, every `model_candidates` model, every hot-reload generation, every admin probe), and NOT on the `SecretVault` handle, because two vaults over one store are still one credential. The background task takes the same lock as the request path, so a request hitting 401 mid-refresh simply awaits the release.
  - *Cross-process*: an advisory `flock` on `<store>.<vault-key>.refresh.lock`, held across the network call and the vault write. A gateway and a `baybo llm probe` share the vault but not the table, and the CLI paths deliberately do **not** take the workspace singleton lock (`crates/baybo/src/singleton.rs` is acquired only by `gateway_cmd` and `prompt_cmd`) — so without this they would both POST the same refresh_token and the loser's `refresh_token_reused` would wipe the vault. Once the lock is held the vault is authoritative: a rotation that landed while we queued is adopted instead of re-spent. The lock is an *aid*, not a dependency — an unopenable lock file or an in-memory store (no on-disk home, hence no peers) logs `openai_subscription_refresh_lock` and proceeds, degrading to the previous racy-but-working behaviour rather than blocking the user's chat. Regression tested against a stub issuer that counts wire hits (`peer_processes_do_not_both_burn_the_refresh_token`, `refresh_works_without_a_lock_path`).

Why 1 hour and not "every minute"? Each refresh can trigger server-side `refresh_token` rotation; spinning the rotation counter for no reason wastes one of the few server-side guarantees we have. 1 hr is well below typical access-token TTL and well above the cost of a single refresh, so the background task always finds a bundle either fresh enough to skip or just barely inside the margin.

Test isolation: `OpenAiSubscriptionCompletionModel::new` takes a `background: BackgroundRefresh` (`Enabled` / `Disabled`) parameter. The factory passes `Enabled` (and `Disabled` for one-shot live-model probes); unit tests pass `Disabled` so the suite doesn't leak spawned tasks across tests. Coordinator state is isolated by the same key that shares it: every test builds its own store, and `MemorySecretStore` mints a fresh `StoreIdentity::Ephemeral` per instance, so each test gets its own coordinator and its own (absent) lock path — the intern table never bleeds between tests.

### Request shape (Responses API)

```jsonc
POST https://chatgpt.com/backend-api/codex/responses
Authorization: Bearer <access_token>
ChatGPT-Account-Id: <account_id>     // when present
OpenAI-Beta: responses=experimental
originator: codex_cli_rs              // anti-bot allowlist; required
Content-Type: application/json
// no User-Agent header is set on the Responses call; only the
// auth.openai.com OAuth endpoints send `User-Agent: baybo/<version>`

{
  "model": "gpt-5",
  "instructions": "<system prompt>",
  "input": [...],                     // ResponseItem[]
  "tools": [...],                     // tool definitions (Responses-API shape)
  "tool_choice": "auto",
  "parallel_tool_calls": true,
  "stream": true,
  "store": false,
  "include": ["reasoning.encrypted_content"]  // only when reasoning effort is set (with "reasoning": {effort, summary: "auto"})
}
```

Response is SSE; we parse `response.output_text.delta`, `response.reasoning*.delta`, `response.output_item.added` / `response.output_item.done`, `response.function_call_arguments.*`, `response.completed`, `response.error` events into `StreamEvent`s.

`originator: codex_cli_rs` is mandatory — Codex's edge rejects requests without it. Yes we're impersonating Codex CLI; the OpenClaw docs note this is the "explicitly supported" external-tool path. We do **not** spoof `User-Agent: codex_cli_rs/...` — the Responses call sends no User-Agent at all, and only the OAuth endpoints carry baybo's own UA. This is the minimum needed for the route, no more.

### Conversion: rig `CompletionRequest` → Codex `ResponsesApiRequest`

`OpenAiSubscriptionCompletionModel` owns the conversion. Mapping:

| rig field | Codex field | Notes |
|---|---|---|
| `preamble` | `instructions` | system prompt |
| `chat_history` (Vec<Message>) | `input` (Vec<ResponseItem>) | each rig `Message` → one or more `ResponseItem`s |
| `tools` | `tools` | translate tool schema to the flat Responses-API shape (`{type: "function", name, description, parameters}` — no nested `function` object) |
| `temperature` / `max_tokens` | dropped | Codex Responses rejects `temperature` with 400 "Unsupported parameter: temperature" (regression-tested: `body_drops_temperature_for_codex_responses`); `max_tokens` is likewise not forwarded |
| (none) | `parallel_tool_calls: true`, `stream: true`, `store: false` | hard-coded |

Tool-call return path: Responses API emits `response.function_call_arguments.delta` events; we accumulate per `call_id` (registered from `response.output_item.added`), finalise on `response.function_call_arguments.done` or `response.output_item.done` (item type `function_call`), surface as `StreamEvent::ToolCall`. Same shape the OpenAI variant already produces.

## Error mapping

| Source | baybo `LlmError` |
|---|---|
| Vault returns None | `Config("openai-subscription: not signed in — add an entry via `baybo llm add` (pick the openai-subscription provider) or re-authenticate an existing entry via `baybo llm edit`")` |
| Bundle JSON parse error | `Config("openai-subscription: vault read failed: ...")` — the entry is **not** auto-cleared; recover via `baybo llm edit` → OAuth login, or `baybo llm remove` |
| Refresh permanent failure | `Config("openai-subscription: refresh token expired/revoked — re-login required")` (auto-clear bundle) |
| Refresh transient failure | surfaced as a rig `CompletionError::ProviderError`, classified to `LlmError::Transient` by `rig_completion_to_error` |
| Responses API 401 after refresh | rig `CompletionError::ProviderError("openai-subscription: unauthorized after refresh — ...")` (completion_model.rs), classified to `LlmError::Transient` by `rig_completion_to_error` |
| Responses API 429 | `RateLimited { retry_after, message }` (via `status_to_error`) — agent's `ErrorHandler` already retries |
| Responses API 5xx | `Transient(...)` (via `status_to_error`) — agent retries through its existing path |
| SSE / response parse error | `Decode(...)` (via `reqwest_to_error`) or a rig `ProviderError` on the streaming path |

## Security & TOS

- **Endpoint allowlist** — *load-bearing trust boundary*: the factory rejects any `base_url` whose host doesn't match `chatgpt.com` (or subdomain) or `auth.openai.com`, returning `LlmError::Config`. The OAuth bearer is technically powerful enough to also authenticate against `api.openai.com/v1/*` or any third-party host, so a malicious or mis-edited `baybo.json` that points `base_url` at an attacker host would otherwise hand the bearer to the attacker on the first LLM call. The allowlist closes that without trusting operators to read the warning.
- **Forward-compat escape hatch**: if OpenAI moves the endpoint to a host outside the allowlist, the operator can set `BAYBO_OPENAI_SUBSCRIPTION_UNSAFE_BASE_URL=1` in the env to bypass validation. The bypass is an env var rather than a JSON field by design — flipping a credential-leak guard should require an explicit shell action, not slip in through a config edit. The bypass also emits a `tracing::warn!(event = "openai_subscription_unsafe_base_url")` so the override is auditable in logs.
- **Lookalike-host hardening**: the suffix match is `host == suffix || host.ends_with(".{suffix}")`, so `chatgpt.com.attacker.example` does NOT match `chatgpt.com`. Regression tested.
- **User-facing transparency**: the `openai_subscription_unsafe_base_url` warn event makes the override state visible in tracing; a richer surface in `baybo llm status` (default-allowed vs allowlisted-non-default vs unsafe-override-active) is future work.
- **Single-flight refresh** — *load-bearing concurrency invariant*: every refresh path (just-in-time, reactive 401, background loop) funnels through `RefreshCoordinator::single_flight_refresh()`, which holds that credential's `flight: Mutex<()>`. After acquiring the lock, the helper re-checks the shared cache against the caller's `RefreshTrigger`: `NearExpiry` is satisfied by *any* bundle outside the margin, `Rejected` only by a *different* `access_token`. Without the re-check, two concurrent paths would both call `refresh()` with the same token; the loser would hit `refresh_token_reused` → permanent failure → vault cleared → user logged out under nothing but normal load. Two traps this has already fallen into, both regression tested:
  - The gate must be shared per **credential**, not per client. It was per client until `model_candidates` shipped, at which point N models over one credential meant N caches and N flight locks that could not see each other — the guarantee silently evaporated for exactly the config the picker encourages. Anchored now by the intern table (`coordinator_is_shared_across_clients_on_one_vault`, `dedup_spans_two_clients`).
  - The re-check predicate must key on *what the caller needed*, not on `refresh_token` inequality. The server may return a fresh `access_token` **without** rotating the `refresh_token` (`oauth.rs`: `body.refresh_token.unwrap_or_else(|| refresh_token.to_string())`), in which case an inequality test can never fire and the dedup is dead code (`near_expiry_trigger_is_satisfied_by_any_fresh_bundle`, `rejected_trigger_requires_a_different_access_token`).
- **HTTPS-only on the bearer transport** *(Codex R2-F2)*: the `base_url` validator rejects any non-HTTPS scheme **before** the host suffix check, so even an allowlisted host on `http://` is refused. Protects the bearer from on-path observers and TLS-decrypting proxies that the host allowlist alone wouldn't catch. The unsafe override env var doesn't relax this — it can only widen the host allowlist, never weaken the scheme requirement. Regression tested (`validate_base_url_rejects_http_with_allowlisted_host` + 3 sibling tests).
- **Durable refresh persistence** *(Codex R2-F3)*: after a successful OAuth refresh, `save_with_retries()` writes the rotated bundle to vault with up to 3 attempts (100ms / 500ms / 2s backoff). If all attempts fail, the bundle is kept in memory but flagged `persisted: false` ("dirty"). The next refresh-path entry retries the save before doing anything else (self-heal); if it STILL can't persist, it refuses to rotate again — better to wait than chain unsaved bundles that all evaporate on process restart. Without this, a transient FS glitch during refresh would silently lose the rotated `refresh_token` and the next process to start would hit `refresh_token_reused` → forced re-login. Regression tested (`save_with_retries_recovers_within_budget`, `save_with_retries_gives_up_after_budget`, `single_flight_refresh_self_heals_dirty_save`, `single_flight_refresh_refuses_to_rotate_when_dirty_save_keeps_failing`).
- **Cross-process logout invalidation** *(Codex R2-F1)*: every cache hit re-validates against the vault on a periodic interval (`CACHE_VAULT_REVALIDATE_INTERVAL_SECS = 60`). Within the window, repeat calls skip the vault read entirely (hot path stays cheap). Past the window, a missing vault entry drops the in-memory cached bundle so a `baybo llm remove` run by another process (which clears the vault entry as part of removal) is honoured within ~60s. Without this, a CLI removal would only delete the on-disk vault entry while a running gateway / TUI keeps using its cached bundle (and 401 reactive refresh would even write a new bundle back into vault, partially undoing the logout). Regression tested (`ensure_fresh_bundle_invalidates_cache_when_vault_is_emptied`, `ensure_fresh_bundle_skips_vault_within_revalidate_interval`).
- **Anti-impersonation**: baybo never spoofs a Codex User-Agent — the Responses call sets no UA at all, and the `auth.openai.com` OAuth endpoints send baybo's own `User-Agent: baybo/<version>`. Only `originator: codex_cli_rs` mimics Codex (mandatory header for the route). Rationale documented inline.
- **Token at rest**: encrypted with the same AES-256-GCM master key as every other vault entry — same blast radius as a stored API key.
- **Token in memory**: cached in the credential's `RefreshCoordinator` for the process lifetime (`CachedBundle` wraps the bundle plus `persisted` / `last_vault_check` bookkeeping); on logout we drop it and delete the vault entry. Refilling that cache from the vault will not overwrite a **dirty** entry with an older bundle — a dirty entry is the only copy of a rotated token that failed to persist, and dropping it would also drop the flag the "refuse to rotate again" guard reads (`vault_hit_does_not_clobber_a_dirty_cached_rotation`).
- **Audit log**: every refresh emits a tracing event with `event=openai_subscription_token_refresh`, `outcome=success|transient|permanent`, no token material in logs ever.

## Testing

- Unit: PKCE codes well-formed; JWT exp parsing handles short/missing claims. The refresh-on-401 retry path is not yet covered by a test (it needs an HTTP mock).
- Unit: `OAuthTokenBundle` round-trips through `SecretVault::store_typed` / `get_typed` (uses existing `MemorySecretStore` test_support).
- Unit: rig→Codex request conversion produces the expected JSON for representative messages (text, tool call, tool result, image stub).
- Behaviour (not asserted by a test): `send()` concatenates only `/codex/responses` onto `base_url` and never silently rewrites the host — with `base_url` unset the request URL is exactly `https://chatgpt.com/backend-api/codex/responses`.
- Integration: a manual live smoke test (real PKCE login + a single chat, env-gated, out of CI) is planned, not yet implemented.
- No mock for the OpenAI auth endpoints in CI — too fragile, the official endpoints are stable enough that contract tests are low-value here.

## Out-of-scope follow-ups (filed for later, not part of B)

1. Multi-profile (e.g. one personal + one workspace account) — vault key would shard to `llm.openai-subscription.profiles.<id>`. `CredentialKey.vault_key` exists for exactly this: populate it from the real key or two profiles collide on one coordinator.
2. Wiring the same `OAuthTokenBundle` into image-generation tool calls
3. Cost tracking — Codex Responses doesn't bill per-token to the user; treat as $0 in `cost` records and document
4. C track: native Codex app-server harness (sidecar)
