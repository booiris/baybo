# cost - Cost Records and Spending Guards

## Overview

The `cost` crate records token usage, cost statistics, and spending enforcement. It does not call LLMs and does not decide business flow — it is assembled by `agent` as pure billing infrastructure.

Core responsibilities:

- Record token usage and USD cost for every LLM call
- Provide cost aggregation by user, globally, and by time range
- Check spending limits before a request enters execution
- Associate records with `job` and `trace` for auditing

## Design Decisions

### CostRecord as smallest auditable unit

Every `CostRecord` must link to both `job_id` and `trace_span_id`, so the system never knows the cost without knowing which call caused it.

### Record only after completion

Cost is recorded only after an actual LLM call successfully completes, to avoid polluting billing data with estimates.

### CostGuard decides, CostTracker records

`CostGuard` checks limits (daily/monthly per-user and global) before execution in Router or AgentLoop. `CostTracker` handles recording and aggregation. They are separate concerns.

### Limit enforcement policy

If any limit is exceeded, reject the new request. Requests already in progress should not be interrupted mid-flight unless the product explicitly requires hard interruption.

### Free local models

Local models (e.g. Ollama) may still record token usage with `cost_usd = 0.0` for observability.

## Constraints

- Depends only on `core`
- Does not depend on `llm` directly — `TokenUsage` and `ModelPricing` are assembled by `agent::ObservabilityRecorder`
- Use sufficient precision for cost calculations to avoid `f64` accumulation errors in financial scenarios

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `ObservabilityRecorder` calls `CostTracker` after successful LLM spans |
| `llm` | Provides model pricing and token usage |
| `job` / `trace` | `CostRecord` links to specific call via `job_id` and `trace_span_id` |
| `hook` | Limit hits can trigger `HookPoint::CostLimitReached` |
