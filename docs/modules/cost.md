# cost - LLM-Call Cost Recording and Budget Gating

## Overview

The `cost` crate is the home for spend-tracking logic: the `CostManager` business-logic facade plus its budget primitives (`SpendingLimits`, `CostGuardError`, `CostMetrics`) and `CostError`. The `CostStore` trait lives in the `aura-store` ports crate; the data types (`CostRecord`, `CostSummary`, `TimeRange`) live in `aura-model`.

`aura-storage` provides the libsql implementation of `CostStore` (the trait itself lives in `aura-store`), so downstream callers and tests can depend on `aura-cost` plus the ports crate for cost-management work.

## Design Decisions

### Integer micro-USD, never floats

Every monetary field — `CostRecord.cost_usd`, `CostSummary.total_cost_usd`, `SpendingLimits.daily_usd` / `monthly_usd` — is `aura_model::MicroUsd` (an `i64` of micro-dollars). Float arithmetic would drift across aggregations and quota checks; the budget gate would be one rounding error away from refusing a call that should have been allowed. Pricing tables (`ModelPricing` from `aura-llm`) are denominated per million tokens; `MicroUsd::cost_for_tokens` does the multiplication in integer space.

### CostManager owns spend side-effects synchronously, persistence async

`CostManager::record_call` is the only entry point for the agent loop. It performs two operations in this order:

1. **Synchronous** in-memory accumulator update against `BudgetState` (today's spend + this month's spend, with lazy rollover on the wall-clock boundary). The next iteration's `CostManager::check` sees the updated total immediately — no race window where two near-simultaneous LLM calls each pass the gate after the limit has been breached.
2. **`tokio::spawn`'d** `CostStore::record` write. Persist failures log a warning and tick `metrics.persist_failures`. They never fail the LLM call.

`Router` also calls `check` at message ingress so over-cap users never spin up an actor.

### `cost_call_guard` bridges to `LlmCallGuard`

`LlmProviderRegistry::create_client` takes a closure-shaped `LlmCallGuard` (from `aura-llm`). `cost_call_guard(&Arc<CostManager>)` produces that closure: it calls `CostManager::check` and maps `CostGuardError` → `LlmError::GuardRejected`. Lives as a free function rather than a `CostManager` method so the manager doesn't have to know about `LlmError`.

### Pricing snapshot reload

The bundled `ModelPricing` snapshot is good enough for first boot, but rates drift. `CostManager::merge_pricings` overlays a freshly-fetched live pricing map without blocking LLM-client wiring on the network fetch; rates differing by more than `PRICING_DRIFT_WARN = 25%` from the bundled snapshot log a `warn!` so operators see a tier rename or list-price cut before it silently distorts the budget.

### Hydration at boot

`CostManager::hydrate` reads today's and this-month's records from disk at process start. Without it, a restart would silently widen the budget — every call would pass the gate until the next persisted record reconciled the in-memory total. Hydration is best-effort; persist failures since the last reconcile show up as `metrics.persist_failures > 0`.

## Constraints

- Depends on `aura-llm` (for `ModelPricing`) — must not be pulled by `aura-storage` to avoid a cycle (`storage → cost → llm → security → storage`). The libsql `CostStore` impl lives in `aura-storage` and depends on `aura-cost`; the chain stops there.
- No dependency on `aura-storage` — the libsql impl converts its own errors at the trait boundary
- `test_support::MemoryCostStore` is gated behind the `test-support` feature so it never ships in release builds. Downstream test crates pull it in via `aura-cost = { workspace = true, features = ["test-support"] }`

## Collaboration

| Module    | Role                                                                                                                |
| --------- | ------------------------------------------------------------------------------------------------------------------- |
| `model`   | Provides `MicroUsd`, `SessionId`, `JobId`, `SpanId`                                                                  |
| `llm`     | Provides `ModelPricing` (rates per 1M tokens) and the `LlmCallGuard` closure shape `cost_call_guard` adapts to       |
| `agent`   | Constructs one `CostManager` per process; calls `record_call` after every LLM span closes, gates ingress with `check` |
| `store`   | Owns the `CostStore` trait contract and `StorageError`                                                              |
| `storage` | Provides the libsql implementation of `CostStore` (trait from `aura-store`)                                          |
