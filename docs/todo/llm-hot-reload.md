# LLM Hot Reload

## Problem

LLM entries (`config.llm[*]` — `provider`, `model`, `base_url`, `api_key_env`, `pricing`, `context_window`, …) only take effect at boot. `src/runtime.rs` builds the `LlmClientPool` once (`LlmClientPool::with_tier_map`) and seeds the `CostManager` pricing map from the built clients right after. The admin endpoints (`crates/gateway/src/api/admin/llm.rs` — `update_model` / add / remove) only `write_to_file` and return "Gateway restart required to pick up". There is no live path that rebuilds the pool.

`docs/todo/config-hot-reload.md` deliberately lists "anything that influences `LlmClient` identity" as **not** hot-updatable, deferring exactly this. So changing a model id today is a restart, full stop.

## Proposed Direction

Build on the `ConfigHandle` / atomic-swap machinery from `config-hot-reload.md` once it lands; LLM identity then moves out of that doc's "not hot-updatable" list into a dedicated rebuild path:

1. **Rebuild + atomic swap** — on reload, rebuild the pool from the new `config.llm` and swap a single `Arc<LlmClientPool>` (handle held via `ArcSwap`/`RwLock`). In-flight requests finish on the old pool; only new requests see the new one. Mirror boot's failure policy: a non-default entry that fails to build is dropped with a warning; the **default** entry failing rejects the reload (no partial swap).
2. **Re-seed `CostManager` pricing (mandatory, not optional)** — after rebuilding, `merge_pricings` from the new clients' `model_info` (`id → pricing`), exactly like the boot seed in `src/runtime.rs`. The cost lookup keys by the active client's `model_info.id` (`crates/agent/src/runtime/billed_chat.rs`); a newly-configured model id absent from the map attributes **$0** → budget gate silently off. This step is what makes the swap safe.
3. **Rebuild the async refresh entry list** — the `fetch_overlay_for` loop captures `config.llm` `(provider, model)` pairs *once* at boot (`src/runtime.rs`, `configured_for_refresh`). On reload it must be restarted/updated with the new pairs, or it keeps refreshing the old models and never the new ones.
4. **Cheap path for pricing-only edits** — a change to just `config.pricing` (no provider/model/base_url change) doesn't need a client rebuild; it can be applied straight through `CostManager::merge_pricings`. Worth special-casing since it's the common operator tweak.

## Related

- `docs/todo/config-hot-reload.md` — the general reload contract; this is the `LlmClient`-identity carve-out it defers. Honor its validation-rollback + atomic-swap rules.
- `src/runtime.rs` — pool build (`with_tier_map`), the boot CostManager seed (`merge_pricings` from clients), and the `fetch_overlay_for` refresh loop. All three need re-running on reload.
- `crates/gateway/src/api/admin/llm.rs` — `update_model` / add / remove; today write config + return "restart required", would instead trigger the reload.
- `crates/agent/src/runtime/billed_chat.rs` — the cost lookup keyed by `model_info.id`, the reason step 2 is mandatory.
