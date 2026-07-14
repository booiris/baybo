# query - Read-only Analytics Surface

## Overview

The `query` crate is the read-only facade over `Session` / `Job` / `Step` / `Span` / `Cost` for admin / CLI / UI consumers. `QueryApi` collapses session + job + trace + cost reads behind a single error type (`QueryError`) so handlers don't match four different store error types.

Thirteen endpoints today:

1. `load_session` — resolves lineage
2. `list_jobs` — job summaries for a session
3. `load_job` — Job + step list
4. `load_step` — Step + spans + span events
5. `find_recoverable_jobs` — recovery scan over non-terminal jobs
6. `list_active_subagents` — live Subagent-lineage children
7. `lineage_tree` — ancestry + immediate descendants
8. `cost_summary` — `User` / `Session` / `Job` / `TimeRange` scope
9. `replay` — chronological Job → Step → Span tree
10. `list_session_summaries` — paginated, filtered per-session aggregates for the admin session browser
11. `compute_analytics` — cost + session-creation aggregates for the analytics dashboard (`Unsupported` without a `CostStore`)
12. `load_trace_overview` — a session's job list + message log once, for the trace sidebar
13. `load_job_trace` — one job's full `steps → spans → events` tree (follow-up to `load_trace_overview`)

## Design Decisions

### One error type, one entry point

Each underlying store has its own error type (`SessionError`, `JobError`, `TraceError`, `CostError`). Handlers don't want to match four enums to map them to HTTP responses, so `QueryError` `#[from]`-wraps each. The CLI and gateway handle `QueryError` alone, stringifying it into their own error types (`CliError` / `GatewayError`).

### Read-only — no mutation methods

`QueryApi` is the only "service" in the read path and it never writes. `list_active_subagents` filters non-terminal jobs by `LineageKind::Subagent`; `find_recoverable_jobs` wraps `JobLifecycle::list_recoverable` (the agent's boot recovery calls the lifecycle directly, not this endpoint). Mutating recovery is the agent layer's responsibility, not the query layer's.

### Carries `Arc<JobLifecycle>`, not raw `JobStore`

The job read path needs `JobLifecycle::list_by_session` / `list_recoverable` (pre-sorted, status-filtered). Using the lifecycle facade rather than the raw store avoids duplicating the sort + filter logic in the query crate.

### `Option<Arc<dyn CostStore>>` so trace-only callers can skip cost wiring

`QueryApi::without_costs` constructs an API without a `CostStore`; `cost_summary` then returns `Unsupported`. The CLI's `ContextBuilder::build` falls back to this constructor when no `CostStore` was wired (crates/cli/src/context.rs); every shell command that needs the query graph (`status --live`, `cost`, `session`, `job`, `cron`) wires the cost store, so `cost_summary` works there.

## Constraints

- No dependency on `baybo-agent` — the query path is pure read, no manager state. The gateway and CLI consume `QueryApi`; `baybo-agent` sits on neither side of the edge.
- Deletes are upstream stores' responsibility — `QueryApi` never calls `record_transition` or any other mutation method.

## Collaboration

| Module    | Role                                                                                                          |
| --------- | ------------------------------------------------------------------------------------------------------------- |
| `store`   | Owns the `SessionStore` / `JobStore` / `TraceStore` / `CostStore` trait contracts, the `StoredMessage` row type, and `StorageError`; `QueryApi` reads through these trait objects |
| `storage` | Provides the sqlite implementations those trait objects resolve to                                             |
| `job`     | `JobLifecycle` facade + domain types (`Job`, `JobStatus`, …); the `JobStore` it wraps is an `baybo-store` trait  |
| `trace`   | `Step`, `Span`, `SpanEvent` + their `from_row` conversions — `QueryApi` rehydrates rows into rich types here    |
| `cost`    | DTOs (`CostSummary`, `TimeRange`); `CostScope` lives here in `query` as the scope enum                          |
| `model`   | `SessionId`, `JobId`, `StepId`, `MicroUsd`, `Lineage`, `LineageKind`                                           |
