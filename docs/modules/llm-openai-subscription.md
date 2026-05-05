# llm-codex-oauth — ChatGPT/Codex OAuth as an LLM provider

## Goal

Let aura users drive `gpt-5`-class models with their **OpenAI ChatGPT/Codex subscription** instead of an `OPENAI_API_KEY`. The HTTP path is `chatgpt.com/backend-api/codex/responses` (Codex Responses API), the credential is a ChatGPT OAuth bearer minted via PKCE against `auth.openai.com`. This is OpenClaw's "Codex OAuth via PI" route — the equivalent for aura's own agent runtime, not the heavier "wrap the Codex CLI app-server" route (that's the C track, deferred).

Public-knowledge inputs: `client_id = app_EMoamEEZ73f0CkXaXp7hrann`, issuer `https://auth.openai.com`, scopes `openid profile email offline_access api.connectors.read api.connectors.invoke`, with the Codex CLI's extra parameters (`id_token_add_organizations=true`, `codex_cli_simplified_flow=true`, `originator=codex_cli_rs`). Source: `openai/codex` repo, `codex-rs/login/`.

## Non-goals (this module)

- Native Codex app-server runtime (track C)
- Image generation through the Codex Responses backend (separate module)
- Multi-account / workspace switching UI — single active profile is enough for v1
- Auth-state sharing with a locally installed `codex` CLI's `~/.codex/auth.json` — explicit re-login, not import. Cleaner trust boundary; matches OpenClaw's stance after they dropped that import path.

## Surface

### New provider id: `openai-subscription`

Selected via `aura.json`:

```json
{
  "llm": {
    "provider": "openai-subscription",
    "model": "gpt-5"
  }
}
```

Naming rationale: this is the explicit "use your OpenAI subscription" path, distinct from `openai` (API-key, pay-per-token billing). The leading `openai-` keeps it grouped with `openai`/`openai-codex`-style ids in `aura llm models` listings and avoids putting a vendor product brand (`chatgpt`) directly in operator-facing config. Open question — could equally be `chatgpt`, `openai-oauth`, or `openai-codex`; final decision deferred to review.

No `api_key_env` is consulted; tokens come from the vault. `base_url` defaults to `https://chatgpt.com/backend-api`. **Default-deny on the bearer destination**: an override is accepted only if the parsed host suffix is on the allowlist (`chatgpt.com` and its subdomains, `auth.openai.com`). Anything else fails at provider construction with `LlmError::Config` so the misconfiguration surfaces at boot rather than leaking the bearer at first request. To deliberately override (operator owns the TOS and credential-leak risk), set the env var `AURA_OPENAI_SUBSCRIPTION_UNSAFE_BASE_URL=1` — env rather than `aura.json` field on purpose, so flipping a bypass requires an explicit shell action.

### Vault entry: `llm.openai-subscription.tokens`

Single canonical key (single profile per aura process). Stored as a typed bundle:

```rust
// in aura-llm/src/providers/openai_subscription/token_bundle.rs
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
// in aura-security/src/secret_vault.rs (new)
impl SecretVault {
    pub async fn store_typed<T: Serialize>(&self, name: &str, value: &T) -> Result<()>;
    pub async fn get_typed<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>>;
}
```

Existing `store_secret` / `get_secret` (raw bytes) stay for symmetric-key style entries; typed accessors are layered on top with `serde_json` and the same AES-GCM encryption underneath. The bundle type itself stays in `aura-llm` — `aura-security` doesn't learn about OAuth.

### CLI surface (folded into `aura llm`)

OAuth login is not a separate `auth` subcommand; it's wired into the existing entry-management flow. Picking the `openai-subscription` provider triggers an interactive login that mirrors what a dedicated `auth login` would do, and removal handles `revoke + clear` symmetrically.

| Command | What it does |
|---|---|
| `aura llm add` (pick `openai-subscription` provider) | Prompts the operator to choose between two login methods: **PKCE** (default on a TTY) opens `https://auth.openai.com/oauth/authorize?...` and runs a one-shot HTTP listener on `127.0.0.1:1455` for the callback; **Device code** (auto-selected on non-TTY stdin, also offered on TTY for headless boxes) hits `/api/accounts/deviceauth/usercode` + polled `/deviceauth/token`. The exchanged bundle is persisted in the vault under a single shared key (one profile per workspace). After the login the flow continues with model + reasoning-effort selection. |
| `aura llm edit` → `OAuth login (re-authenticate)` | Re-runs the same PKCE / device-code dialog and overwrites the vault bundle. Used to recover from a stale refresh token without removing the entry. |
| `aura llm remove` (entry whose provider is `openai-subscription`) | Deletes the config entry; if no other `openai-subscription` entries remain, also calls `/oauth/revoke` (RFC 7009, best-effort — logs but does not fail the command) and clears the vault entry. **Vault clear failure is logged but not fatal**: the config entry is gone, so the runtime no longer routes through this provider; the next process won't see the token. Server-side revoke success is reflected in the JSON output (`subscription_revoked: true|false`). |
| `aura llm status` | Lists every registered entry (name / provider / model / api_key_env). It does **not** surface OAuth token state (expiry, account email, plan type, override state) today — that data is printed once at login time and is otherwise only visible in tracing events. A richer status view is future work. |

All of the above lives in `crates/cli/src/commands/llm.rs`. The OAuth surface itself (`pkce_login`, `device_code_login`, `revoke`, `VaultTokenStore`) is exposed by `aura_llm::providers::openai_subscription` for the CLI to consume.

## Architecture

### New crate-private module: `crates/llm/src/providers/openai_subscription/`

```
openai_subscription/
├── mod.rs              — pub use; OpenAiSubscriptionProviderFactory
├── token_bundle.rs     — OAuthTokenBundle, JWT parsing (exp + chatgpt_account_id)
├── token_store.rs      — VaultBackedTokenStore (SecretVault wrapper)
├── oauth.rs            — pkce_flow(), device_code_flow(), refresh()
└── completion_model.rs — OpenAiSubscriptionCompletionModel: rig::CompletionModel impl
```

Module directory and inner struct both use `openai_subscription` / `OpenAiSubscriptionCompletionModel` to match the user-facing provider id; the wire format (Codex Responses API) and impersonation surface (`originator: codex_cli_rs`) are documented in the module's header comment but don't leak into struct names.

Why split: `oauth.rs` and the CLI both need `token_bundle` + `token_store`, but the agent's hot path only needs `completion_model`. Also makes the OAuth bits independently testable.

### `AnyCompletionModel` change

`crates/llm/src/lib.rs:262` is currently:

```rust
pub(crate) enum AnyCompletionModel {
    OpenAI(openai::completion::CompletionModel),
    Anthropic(anthropic::completion::CompletionModel),
    Gemini(gemini::completion::CompletionModel),
}
```

Add a fourth variant:

```rust
OpenAiSubscription(crate::providers::openai_subscription::OpenAiSubscriptionCompletionModel),
```

`OpenAiSubscriptionCompletionModel` implements rig's `CompletionModel` trait directly (using its public type signatures) so the existing match arms in `completion()` and `stream()` get a uniform 4th arm. We do **not** route through `rig::providers::openai` because:

1. The wire format is the Responses API (`input` / `instructions` / `tools` / `reasoning` / `store=false`), not Chat Completions (`messages`).
2. `Authorization: Bearer <ChatGPT JWT>` plus `ChatGPT-Account-Id: <id>` is required; rig's openai client only knows API-key bearers.
3. We need 401 → refresh-and-retry. rig won't.

### Token store wiring — `aura-llm` depends on `aura-security`

`aura-llm/Cargo.toml` gets `aura-security = { workspace = true }` added. This is the simplest plumbing path and matches the user-stated decision over the more ceremonial "register codex factory separately" alternative I'd previously considered.

Concrete consequences:

- `LlmProviderConfig` grows an `Option<Arc<SecretVault>>` field. For the openai-subscription factory it MUST be `Some` (returns `LlmError::Config` otherwise); for every other provider it's ignored.
- `with_default_providers()` registers the openai-subscription factory like all the others — boot doesn't have to do anything special.
- `boot::build_llm_client` gains a `vault: Arc<SecretVault>` parameter (touches every test fixture that builds an `LlmClient` — accepted cost per user).
- The factory's `create()` reads the bundle synchronously off the vault via a `tokio::runtime::Handle::current().block_on(...)` — `LlmProviderFactory::create` is sync and the vault accessor is async. Acceptable because boot runs inside a tokio runtime; if that ever changes we expose an async factory variant.

`aura-llm` already depends on `tokio` for its streams, so the extra `aura-security` edge doesn't pull in a new ecosystem.

### Refresh policy

Three layers, in order of how often they fire:

- **Background (proactive)**: a tokio task spawned at provider construction wakes every `BACKGROUND_REFRESH_INTERVAL_SECS = 3600` (1 hour). If the cached bundle is within `BACKGROUND_REFRESH_MARGIN_SECS = 300` (5 min) of expiry, refresh proactively. This keeps a long-idle process from making the next user-facing call pay the refresh latency. Loop exits gracefully when the vault is empty (user logged out) or a permanent refresh error fires (token revoked); the next sign-in re-spawns it.
- **Pre-flight (just-in-time)**: if `expires_at < now() + REFRESH_SKEW_SECS (60)` when a request is about to go out, refresh before sending. Safety net for the ~1 hr window the background task could be late on (process just started, system clock skew, background loop hasn't ticked yet).
- **Reactive (401)**: 401 from `/codex/responses` → refresh once, retry once. Second 401 surfaces as `LlmError::Provider("openai-subscription: unauthorized after refresh")` — the user must re-login.
- **Refresh failure**: `refresh_token_expired` / `refresh_token_reused` / `refresh_token_invalidated` → return a typed error and clear the vault entry (so a stale bundle doesn't keep failing). Other 4xx from refresh → transient error, don't clear, surface to caller (or just log + retry-next-tick if the background task hit it).
- **Concurrency**: a `tokio::sync::Mutex` around the cached token bundle; only one in-flight refresh per process. The background task takes the same lock as the request path, so a request hitting 401 while the background refresh is in progress simply awaits the same lock release.

Why 1 hour and not "every minute"? Each refresh can trigger server-side `refresh_token` rotation; spinning the rotation counter for no reason wastes one of the few server-side guarantees we have. 1 hr is well below typical access-token TTL and well above the cost of a single refresh, so the background task always finds a bundle either fresh enough to skip or just barely inside the margin.

Test isolation: `OpenAiSubscriptionCompletionModel::new` takes an `enable_background_refresh: bool`. The factory passes `true`; unit tests pass `false` so the suite doesn't leak spawned tasks across tests.

### Request shape (Responses API)

```jsonc
POST https://chatgpt.com/backend-api/codex/responses
Authorization: Bearer <access_token>
ChatGPT-Account-Id: <account_id>     // when present
OpenAI-Beta: responses=experimental
originator: codex_cli_rs              // anti-bot allowlist; required
User-Agent: aura/<version> (...)
Content-Type: application/json

{
  "model": "gpt-5",
  "instructions": "<system prompt>",
  "input": [...],                     // ResponseItem[]
  "tools": [...],                     // tool definitions (Responses-API shape)
  "tool_choice": "auto",
  "parallel_tool_calls": true,
  "stream": true,
  "store": false,
  "include": ["reasoning.encrypted_content"]
}
```

Response is SSE; we parse `response.output_text.delta`, `response.function_call.*`, `response.completed`, `response.error` events into `StreamEvent`s.

`originator: codex_cli_rs` is mandatory — Codex's edge rejects requests without it. Yes we're impersonating Codex CLI; the OpenClaw docs note this is the "explicitly supported" external-tool path. We do **not** spoof `User-Agent: codex_cli_rs/...`; aura sends its own UA. This is the minimum needed for the route, no more.

### Conversion: rig `CompletionRequest` → Codex `ResponsesApiRequest`

`OpenAiSubscriptionCompletionModel` owns the conversion. Mapping:

| rig field | Codex field | Notes |
|---|---|---|
| `preamble` | `instructions` | system prompt |
| `chat_history` (Vec<Message>) | `input` (Vec<ResponseItem>) | each rig `Message` → one or more `ResponseItem`s |
| `tools` | `tools` | translate tool schema to Responses-API shape (`type: "function"`, `function: {name, description, parameters}`) |
| `temperature` / `max_tokens` | passed through | gpt-5 ignores most of these; harmless |
| (none) | `parallel_tool_calls: true`, `stream: true`, `store: false` | hard-coded |

Tool-call return path: Responses API emits `response.function_call_arguments.delta` events; we accumulate per `id`, finalise on `response.function_call.completed`, surface as `StreamEvent::ToolCall`. Same shape the OpenAI variant already produces.

## Error mapping

| Source | aura `LlmError` |
|---|---|
| Vault returns None | `Config("openai-subscription: not signed in — add an entry via `aura llm add` (pick the openai-subscription provider) or re-authenticate an existing entry via `aura llm edit`")` |
| Bundle JSON parse error | `Config("openai-subscription: corrupt token bundle")` (then auto-clear vault entry) |
| Refresh permanent failure | `Config("openai-subscription: refresh token expired/revoked — re-login required")` (auto-clear bundle) |
| Refresh transient failure | `Provider("openai-subscription: token refresh transient: <err>")` |
| Responses API 401 after refresh | `Provider("openai-subscription: unauthorized after refresh")` |
| Responses API 429 | `RateLimit` (existing variant) — agent's `ErrorHandler` already retries |
| Responses API 5xx | `Provider("...")` — agent retries through its existing path |
| SSE parse error | `Provider("openai-subscription: malformed SSE: <err>")` |

## Security & TOS

- **Endpoint allowlist** — *load-bearing trust boundary*: the factory rejects any `base_url` whose host doesn't match `chatgpt.com` (or subdomain) or `auth.openai.com`, returning `LlmError::Config`. The OAuth bearer is technically powerful enough to also authenticate against `api.openai.com/v1/*` or any third-party host, so a malicious or mis-edited `aura.json` that points `base_url` at an attacker host would otherwise hand the bearer to the attacker on the first LLM call. The allowlist closes that without trusting operators to read the warning.
- **Forward-compat escape hatch**: if OpenAI moves the endpoint to a host outside the allowlist, the operator can set `AURA_OPENAI_SUBSCRIPTION_UNSAFE_BASE_URL=1` in the env to bypass validation. The bypass is an env var rather than a JSON field by design — flipping a credential-leak guard should require an explicit shell action, not slip in through a config edit. The bypass also emits a `tracing::warn!(event = "openai_subscription_unsafe_base_url")` so the override is auditable in logs.
- **Lookalike-host hardening**: the suffix match is `host == suffix || host.ends_with(".{suffix}")`, so `chatgpt.com.attacker.example` does NOT match `chatgpt.com`. Regression tested.
- **User-facing transparency**: the `openai_subscription_unsafe_base_url` warn event makes the override state visible in tracing; a richer surface in `aura llm status` (default-allowed vs allowlisted-non-default vs unsafe-override-active) is future work.
- **Single-flight refresh** — *load-bearing concurrency invariant*: every refresh path (just-in-time, reactive 401, background loop) funnels through one `do_single_flight_refresh()` helper that holds a process-wide `refresh_flight: Mutex<()>`. After acquiring the lock, the helper re-checks the cache: if the cached `refresh_token` differs from what the caller planned to send, someone else already rotated it and we reuse their bundle without a network call. Without the re-check, two concurrent paths would both call `refresh()` with the same token; the loser would hit `refresh_token_reused` → permanent failure → vault cleared → user logged out under nothing but normal load. Regression tested (`single_flight_refresh_dedups_after_concurrent_rotation`).
- **HTTPS-only on the bearer transport** *(Codex R2-F2)*: the `base_url` validator rejects any non-HTTPS scheme **before** the host suffix check, so even an allowlisted host on `http://` is refused. Protects the bearer from on-path observers and TLS-decrypting proxies that the host allowlist alone wouldn't catch. The unsafe override env var doesn't relax this — it can only widen the host allowlist, never weaken the scheme requirement. Regression tested (`validate_base_url_rejects_http_with_allowlisted_host` + 3 sibling tests).
- **Durable refresh persistence** *(Codex R2-F3)*: after a successful OAuth refresh, `save_with_retries()` writes the rotated bundle to vault with up to 3 attempts (100ms / 500ms / 2s backoff). If all attempts fail, the bundle is kept in memory but flagged `persisted: false` ("dirty"). The next refresh-path entry retries the save before doing anything else (self-heal); if it STILL can't persist, it refuses to rotate again — better to wait than chain unsaved bundles that all evaporate on process restart. Without this, a transient FS glitch during refresh would silently lose the rotated `refresh_token` and the next process to start would hit `refresh_token_reused` → forced re-login. Regression tested (`save_with_retries_recovers_within_budget`, `save_with_retries_gives_up_after_budget`, `single_flight_refresh_self_heals_dirty_save`, `single_flight_refresh_refuses_to_rotate_when_dirty_save_keeps_failing`).
- **Cross-process logout invalidation** *(Codex R2-F1)*: every cache hit re-validates against the vault on a periodic interval (`CACHE_VAULT_REVALIDATE_INTERVAL_SECS = 60`). Within the window, repeat calls skip the vault read entirely (hot path stays cheap). Past the window, a missing vault entry drops the in-memory cached bundle so a `aura llm remove` run by another process (which clears the vault entry as part of removal) is honoured within ~60s. Without this, a CLI removal would only delete the on-disk vault entry while a running gateway / TUI keeps using its cached bundle (and 401 reactive refresh would even write a new bundle back into vault, partially undoing the logout). Regression tested (`ensure_fresh_bundle_invalidates_cache_when_vault_is_emptied`, `ensure_fresh_bundle_skips_vault_within_revalidate_interval`).
- **Anti-impersonation**: aura sets `User-Agent: aura/...`. Only `originator: codex_cli_rs` mimics Codex (mandatory header for the route). Rationale documented inline.
- **Token at rest**: encrypted with the same AES-256-GCM master key as every other vault entry — same blast radius as a stored API key.
- **Token in memory**: kept in an `Arc<RwLock<OAuthTokenBundle>>` for the process lifetime; on logout we zero it (best-effort) and delete the vault entry.
- **Audit log**: every refresh emits a tracing event with `event=oai_subscription_token_refresh`, `outcome=success|transient|permanent`, `account_id` (hashed), no token material in logs ever.

## Testing

- Unit: PKCE codes well-formed; JWT exp parsing handles short/missing claims; refresh-on-401 path issues exactly one retry.
- Unit: `OAuthTokenBundle` round-trips through `SecretVault::store_typed` / `get_typed` (uses existing `MemorySecretStore` test_support).
- Unit: rig→Codex request conversion produces the expected JSON for representative messages (text, tool call, tool result, image stub).
- Unit: with `base_url` unset, the constructed request URL is exactly `https://chatgpt.com/backend-api/codex/responses`. With `base_url = Some("https://example.test/foo")`, the constructed URL is exactly `https://example.test/foo/codex/responses` — the module concatenates only `/codex/responses`, never silently rewrites the host.
- Integration (gated by `OPENAI_SUBSCRIPTION_LIVE_TEST=1`): a manual smoke test that runs a real PKCE login and a single chat. Not part of CI.
- No mock for the OpenAI auth endpoints in CI — too fragile, the official endpoints are stable enough that contract tests are low-value here.

## Out-of-scope follow-ups (filed for later, not part of B)

1. Multi-profile (e.g. one personal + one workspace account) — vault key would shard to `llm.openai-subscription.profiles.<id>`
2. Wiring the same `OAuthTokenBundle` into image-generation tool calls
3. Cost tracking — Codex Responses doesn't bill per-token to the user; treat as $0 in `cost` records and document
4. C track: native Codex app-server harness (sidecar)
