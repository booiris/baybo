# Wire Remaining Config Sections into Bootstrap

## Problem

`AuraConfig` validates four sections today that bootstrap does not yet consume. `validate()` enforces their shape, but nothing in `src/main.rs` or `src/boot.rs` reads the values, so operator changes are silently ignored. See `docs/modules/bootstrap.md` §"What boot does NOT do".

| Section | Status | What's missing |
|---------|--------|----------------|
| `channels.http` / `channels.telegram` / `channels.discord` | validated (enabled≠false, required fields) | No adapter is registered with `ChannelRegistry` today. The TUI is a gateway client (`aura-tui`) rather than a registered channel, so these sections currently have no consumer at all. |
| `cost.spending_limits` / `cost.rate_limit` | validated (positivity, cross-field) | `CostTracker` is constructed but `CostGuard` / `RateLimiter` (already implemented per commit 769dd50) aren't wired into the router's request path. |

> **MCP** moved out of `aura.json` entirely. Server records live in
> `<workspace>/.mcp.json`, are owned by `aura-tools::mcp`, and bootstrap
> wires the `McpReconciler` instead of registering servers itself. See
> `docs/modules/tools.md` for the MCP client architecture.

## Proposed Direction

One commit per section, each following the established pattern in `boot.rs`:

1. **Optional channels** — if `config.channels.http.is_some()` etc., construct the corresponding adapter with its config and register it on the gateway's `ChannelRegistry`. Bot tokens come from env vars named by `bot_token_env`.
2. **Cost guard + rate limiter** — build both from `config.cost` and pass into `Router::new` (or a `with_cost_guard` / `with_rate_limiter` chainable setter, matching the existing `with_actor_spawner` / `with_cron_triggers` style).

Each step adds unit tests to `boot::tests` for the pure mapping and covers the wiring with an integration test where feasible.

## Related

- `docs/modules/config.md` §"Section boundaries" — authoritative mapping table
- `docs/modules/bootstrap.md` §"What boot does NOT do"
- `src/boot.rs` — extension point for new `to_*` / `build_*` functions
- `crates/config/src/validate.rs` — validation already in place, safe to trust the shapes
