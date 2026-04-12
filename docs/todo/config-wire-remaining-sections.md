# Wire Remaining Config Sections into Bootstrap

## Problem

`AuraConfig` validates four sections today that bootstrap does not yet consume. `validate()` enforces their shape, but nothing in `src/main.rs` or `src/boot.rs` reads the values, so operator changes are silently ignored. See `docs/modules/bootstrap.md` §"What boot does NOT do".

| Section | Status | What's missing |
|---------|--------|----------------|
| `sandbox` | validated | No `SandboxLimits` / `NetworkPolicy` is constructed anywhere in `main.rs`. Sandbox execution path exists in `aura-sandbox` but isn't reachable from the running router. |
| `tools.mcp_servers[]` | validated (name uniqueness, URL scheme, host ∈ network allowlist, trust/capability matrix) | `ToolRegistry` is created empty; no MCP server is registered from config. |
| `channels.http` / `channels.telegram` / `channels.discord` | validated (enabled≠false, required fields) | Only `CliAdapter` is registered. The other adapters exist in `aura-channels` but bootstrap never picks them up. |
| `cost.spending_limits` / `cost.rate_limit` | validated (positivity, cross-field) | `CostTracker` is constructed but `CostGuard` / `RateLimiter` (already implemented per commit 769dd50) aren't wired into the router's request path. |

## Proposed Direction

One commit per section, each following the established pattern in `boot.rs`:

1. **Sandbox** — add `boot::to_sandbox_limits(&SandboxConfig)` and `boot::to_network_policy(&SandboxConfig)`; construct the sandbox in `main.rs` and inject into `ToolExecutor` (or wherever the sandbox boundary lives once it's plumbed).
2. **MCP servers** — iterate `config.tools.mcp_servers`, map each `McpServerEntry` (via existing mirror types) into the `aura-tools` MCP registration API, and populate `ToolRegistry` before the router starts. Failures should surface as startup errors, not silent skips.
3. **Optional channels** — if `config.channels.http.is_some()` etc., construct the corresponding adapter with its config and register it alongside `CliAdapter`. Bot tokens come from env vars named by `bot_token_env`.
4. **Cost guard + rate limiter** — build both from `config.cost` and pass into `Router::new` (or a `with_cost_guard` / `with_rate_limiter` chainable setter, matching the existing `with_actor_spawner` / `with_cron_triggers` style).

Each step adds unit tests to `boot::tests` for the pure mapping and covers the wiring with an integration test where feasible.

## Related

- `docs/modules/config.md` §"Section boundaries" — authoritative mapping table
- `docs/modules/bootstrap.md` §"What boot does NOT do"
- `src/boot.rs` — extension point for new `to_*` / `build_*` functions
- `crates/config/src/validate.rs` — validation already in place, safe to trust the shapes
