# query - Read-only Analytics Surface

## Overview

The `query` crate is the read-only facade over `Session` / `Turn` / `Step` / `Span` / `Cost` for admin / CLI / UI consumers. `QueryApi` collapses session + turn + trace + cost reads behind a single error type (`QueryError`) so handlers don't match four different store error types.

Fourteen endpoints today:

1. `load_session` — resolves lineage
2. `list_turns` — turn summaries for a session
3. `load_turn` — Turn + step list
4. `load_step` — Step + spans + span events
5. `find_recoverable_turns` — recovery scan over non-terminal turns
6. `list_active_subagents` — live Subagent-lineage children
7. `lineage_tree` — ancestry + immediate descendants
8. `cost_summary` — `User` / `Session` / `Turn` / `TimeRange` scope
9. `replay` — chronological Turn → Step → Span tree
10. `list_session_summaries` — paginated, filtered per-session aggregates for the admin session browser
11. `compute_analytics` — cost + session-creation aggregates, including model, call-reason, and request reasoning-effort breakdowns, for the analytics dashboard (`Unsupported` without a `CostStore`)
12. `load_trace_overview` — a session's turn list + message log once, for the trace sidebar
13. `load_turn_trace` — one turn's full `steps → spans → events` tree (follow-up to `load_trace_overview`)
14. `trace_counts` — per-session `(turns, steps, spans)` tally via SQL counts (CLI `session show`)

## Design Decisions

### One error type, one entry point

Each underlying store has its own error type (`SessionError`, `TurnError`, `TraceError`, `CostError`). Handlers don't want to match four enums to map them to HTTP responses, so `QueryError` `#[from]`-wraps each. The CLI and gateway handle `QueryError` alone, stringifying it into their own error types (`CliError` / `GatewayError`).

### Read-only — no mutation methods

`QueryApi` is the only "service" in the read path and it never writes. `list_active_subagents` filters non-terminal turns by `LineageKind::Subagent`; `find_recoverable_turns` wraps `TurnLifecycle::list_recoverable` (the agent's boot recovery calls the lifecycle directly, not this endpoint). Mutating recovery is the agent layer's responsibility, not the query layer's.

### Carries `Arc<TurnLifecycle>`, not raw `TurnStore`

The turn read path needs `TurnLifecycle::list_by_session` / `list_recoverable` / `session_turn_stats` (pre-sorted, status-filtered, grouped). Using the lifecycle facade rather than the raw store avoids duplicating the sort + filter logic in the query crate.

### `list_session_summaries` paginates before aggregating

The trace-browser listing must scale with page size, not total history. Everything needed pre-pagination — has-turns, latest turn's `status_kind`, turn count — comes from one session scan plus one grouped `TurnStore::session_turn_stats` query. Per-session aggregates that need further store reads (the full latest `TurnStatus`, span counts via `TraceStore::trace_counts_by_turn`, token totals via `CostStore::query_session`) run for the returned page only. The row title prefers `Session.title`; untitled rows derive a one-line, `MAX_SESSION_TITLE_LEN`-bounded fallback from the earliest text-bearing turn input already loaded for the page, so the list does not read full transcripts. Span counting never materialises span `data` blobs — those inline LLM/tool payloads run to hundreds of KB each, and the listing needs only the number.

### Trace trees assemble from batched reads

`load_turn_trace` / `replay` / `load_step` build their `steps → spans → events` trees from grouped store queries — `TraceStore::list_spans_by_turn` (one JOIN through `steps`) and `TraceStore::list_span_events_for_spans` (one IN-list query for exactly the spans missing inline events) — and group in memory. Per-step / per-span store round-trips are off the table: a large turn's tree is a fixed handful of queries, not O(steps + spans) pool checkouts.

### `load_trace_overview` supports incremental polling

The overview accepts `since_ordinal`; when set, `session_messages` carries only rows above it while `turns` always ships in full. The response's `supersede_watermark` (the session's highest `superseded_by` marker) is the staleness signal: it advances only when a compaction re-marks rows the client already holds, telling the poller to drop its cache and do one full reload. Per-turn token chips come from one grouped `CostStore::query_session_by_turn` query, not a `query_turn` per turn.

### `Option<Arc<dyn CostStore>>` so trace-only callers can skip cost wiring

`QueryApi::without_costs` constructs an API without a `CostStore`; `cost_summary` then returns `Unsupported`. The CLI's `ContextBuilder::build` falls back to this constructor when no `CostStore` was wired (crates/cli/src/context.rs); every shell command that needs the query graph (`status --live`, `cost`, `session`, `turn`, `cron`) wires the cost store, so `cost_summary` works there.

## Constraints

- No dependency on `baybo-agent` — the query path is pure read, no manager state. The gateway and CLI consume `QueryApi`; `baybo-agent` sits on neither side of the edge.
- Deletes are upstream stores' responsibility — `QueryApi` never calls `record_transition` or any other mutation method.

## Collaboration

| Module    | Role                                                                                                          |
| --------- | ------------------------------------------------------------------------------------------------------------- |
| `store`   | Owns the `SessionStore` / `TurnStore` / `TraceStore` / `CostStore` trait contracts, the `StoredMessage` row type, and `StorageError`; `QueryApi` reads through these trait objects |
| `storage` | Provides the sqlite implementations those trait objects resolve to                                             |
| `turn`    | `TurnLifecycle` facade + domain types (`Turn`, `TurnStatus`, …); the `TurnStore` it wraps is an `baybo-store` trait |
| `trace`   | `Step`, `Span`, `SpanEvent` + their `from_row` conversions — `QueryApi` rehydrates rows into rich types here    |
| `cost`    | DTOs (`CostSummary`, `TimeRange`); `CostScope` lives here in `query` as the scope enum                          |
| `model`   | `SessionId`, `TurnId`, `StepId`, `MicroUsd`, `Lineage`, `LineageKind`                                          |
