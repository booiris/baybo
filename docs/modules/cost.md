# cost - LLM-Call Cost Recording and Budget Gating

## Overview

The `cost` crate is the home for spend-tracking logic: the `CostManager` business-logic facade plus its budget primitives (`SpendingLimits`, `CostGuardError`, `CostMetrics`) and `CostError`. The `CostStore` trait lives in the `baybo-store` ports crate; the data types (`CostRecord`, `CostSummary`, `TimeRange`) live in `baybo-model`.

`baybo-storage` provides the libsql implementation of `CostStore` (the trait itself lives in `baybo-store`), so downstream callers and tests can depend on `baybo-cost` plus the ports crate for cost-management work.

## Design Decisions

### Integer micro-USD, never floats

Every monetary field — `CostRecord.cost_usd`, `CostSummary.total_cost_usd`, `SpendingLimits.daily_usd` / `monthly_usd` — is `baybo_model::MicroUsd` (an `i64` of micro-dollars). Float arithmetic would drift across aggregations and quota checks; the budget gate would be one rounding error away from refusing a call that should have been allowed. Pricing tables (`ModelPricing` from `baybo-llm`) are denominated per million tokens; `MicroUsd::cost_for_tokens` does the multiplication in integer space.

### CostManager owns spend side-effects synchronously, persistence async

`CostManager::record_call` is the recorder half of the `CostHooks` bundle — `BillableLlm` invokes it after every provider call (the agent loop never calls it directly). It performs two operations in this order:

1. **Synchronous** in-memory accumulator update against `BudgetState` (today's spend + this month's spend, with lazy rollover on the wall-clock boundary). The next iteration's `CostManager::check` sees the updated total immediately — no race window where two near-simultaneous LLM calls each pass the gate after the limit has been breached.
2. **`tokio::spawn`'d** `CostStore::record` write. Persist failures log a warning and tick `metrics.persist_failures`. They never fail the LLM call.

`record_external_tokens` is the one other recording entry point: the subagent spawner logs subscription-billed external-agent runs (claude code Max / codex) at `cost_usd = MicroUsd::ZERO` — tokens are persisted for the analytics breakdowns but never touch the daily/monthly accumulators.

`Router` also calls `check` at message ingress so over-cap users never spin up an actor.

### `cost_call_guard` bridges to `LlmCallGuard`

`BillableLlm` admits every provider call through a closure-shaped `LlmCallGuard` (from `baybo-llm`); it reaches `LlmProviderRegistry::create_client` as the `guard` field of the `CostHooks` parameter. `cost_call_guard(&Arc<CostManager>)` produces that closure: it calls `CostManager::check` and maps `CostGuardError` → `LlmError::GuardRejected`. Lives as a free function rather than a `CostManager` method so the manager doesn't have to know about `LlmError`.

The production wiring is `cost_hooks(&Arc<CostManager>)`, which bundles `cost_call_guard` (the admission guard) with the `record_call` recorder closure into the `baybo_llm::CostHooks` every `BillableLlm` is built with; argv one-shots and tests use `CostHooks::passthrough` instead.

### Pricing snapshot reload

The bundled `ModelPricing` snapshot is good enough for first boot, but rates drift. `CostManager::merge_pricings` overlays a freshly-fetched live pricing map without blocking LLM-client wiring on the network fetch; rates differing by more than `PRICING_DRIFT_WARN = 25%` from the bundled snapshot log a `warn!` so operators see a tier rename or list-price cut before it silently distorts the budget.

### Hydration at boot

`CostManager::hydrate` reads today's and this-month's records from disk at process start. Without it, a restart would silently widen the budget — every call would pass the gate until the next persisted record reconciled the in-memory total. Hydration is best-effort; persist failures since the last reconcile show up as `metrics.persist_failures > 0`.

## Constraints

- Depends on `baybo-llm` (for `ModelPricing`); `baybo-storage` must not pull `baybo-cost` — the storage adapter stays below the domain layer. The libsql `CostStore` impl (`LibsqlCostStore`) lives in `baybo-storage` and implements the `baybo-store` trait over `baybo-model` types only, so no edge from storage back into cost/llm exists.
- No dependency on `baybo-storage` — the libsql impl converts its own errors at the trait boundary
- `test_support::MemoryCostStore` is gated behind the `test-support` feature so it never ships in release builds. Downstream test crates pull it in via `baybo-cost = { workspace = true, features = ["test-support"] }`

## Collaboration

| Module    | Role                                                                                                                |
| --------- | ------------------------------------------------------------------------------------------------------------------- |
| `model`   | Provides `MicroUsd`, `SessionId`, `JobId`, `SpanId`                                                                  |
| `llm`     | Provides `ModelPricing` (rates per 1M tokens) and the `LlmCallGuard` closure shape `cost_call_guard` adapts to       |
| `agent`   | Gates `Router` ingress with `check`; binds per-call `Attribution` so `BillableLlm`'s recorder lands spend on the right span; records subscription-billed external-agent runs via `record_external_tokens` |
| `baybo` (runtime) | Constructs one `CostManager` per process (`runtime.rs`), awaits `hydrate` before any actor can spawn, and wires `cost_hooks` into every `BillableLlm` |
| `store`   | Owns the `CostStore` trait contract and `StorageError`                                                              |
| `storage` | `LibsqlCostStore` implements the `CostStore` trait (from `baybo-store`) over `baybo-model` types; no dependency on `baybo-cost`  |
