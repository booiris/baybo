# query - Read-only Analytics Surface

## Overview

The `query` crate is the read-only facade over `Session` / `Job` / `Step` / `Span` / `Cost` for admin / CLI / UI consumers. `QueryApi` collapses session + job + trace + cost reads behind a single error type (`QueryError`) so handlers don't match four different store error types.

Nine endpoints today:

1. `load_session` — resolves lineage
2. `list_jobs` — fork-prefix UNION + `is_inherited` flag
3. `load_job` — Job + step list
4. `load_step` — Step + spans + span events
5. `find_recoverable_jobs` — recovery scan over non-terminal jobs
6. `list_active_subagents` — live Subagent-lineage children
7. `lineage_tree` — ancestry + immediate descendants
8. `cost_summary` — `User` / `Session` / `Job` / `TimeRange` scope
9. `replay` — chronological Job → Step → Span tree (also the backend for fork's view-layer UNION)

## Design Decisions

### One error type, one entry point

Each underlying store has its own error type (`JobError`, `TraceError`, `CostError`, `StorageError`). Handlers don't want to match four enums to map them to HTTP responses, so `QueryError` `#[from]`-wraps each. The CLI and gateway pattern-match on `QueryError` alone.

### Read-only — no mutation methods

`QueryApi` is the only "service" in the read path and it never writes. `list_active_subagents` filters non-terminal jobs by `LineageKind::Subagent`; recovery scans use `find_recoverable_jobs`. Mutating recovery is the agent layer's responsibility, not the query layer's.

### Carries `Arc<JobLifecycle>`, not raw `JobStore`

The job read path needs `JobLifecycle::list` (pre-sorted, status-filtered). Using the lifecycle facade rather than the raw store avoids duplicating the sort + filter logic in the query crate.

### `Option<Arc<dyn CostStore>>` so trace-only callers can skip cost wiring

`QueryApi::new_trace_only` constructs an API without a `CostStore`; `cost_summary` then returns `Unsupported`. CLI `aura trace …` commands use this path so they don't need to open the cost table.

## Constraints

- No dependency on `aura-agent` — the query path is pure read, no manager state. `aura-agent` consumes `QueryApi`, not the other way around.
- Deletes are upstream stores' responsibility — `QueryApi` never calls `record_transition` or any other mutation method.

## Collaboration

| Module    | Role                                                                                                          |
| --------- | ------------------------------------------------------------------------------------------------------------- |
| `job`     | `JobLifecycle`, domain types, `JobStore` (consumed via the lifecycle facade)                                   |
| `trace`   | `Step`, `Span`, `SpanEvent`, `TraceStore` (raw)                                                                |
| `cost`    | `CostStore` + DTOs (`CostSummary`, `TimeRange`); `CostScope` lives here in `query` as the scope enum            |
| `storage` | `SessionStore`, `StoredMessage`, `StorageError` — the session read surface                                     |
| `model`   | `SessionId`, `JobId`, `StepId`, `MicroUsd`, `Lineage`, `LineageKind`                                           |
