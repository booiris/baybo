# Trace Redesign — done, follow-ups remaining

The full design is implemented and reflected in:

- [`docs/modules/session.md`](../modules/session.md) — Session, lineage, trigger, fork rejection
- [`docs/modules/job.md`](../modules/job.md) — Job state machine (`Completed { verification }`, `Cancelled`, etc.)
- [`docs/modules/trace.md`](../modules/trace.md) — Step / Span / SpanEvent

These specs are authoritative; this file no longer carries the active design.

## Follow-ups

Tracked here so they don't get lost. Each is a scoped extension, not a redesign.

### Subagent runtime bootstrap wiring

`AgentLoop.with_subagent_runtime(...)` is unset in `src/runtime.rs`. With it
unset, `spawn_subagent` returns a graceful `"no subagent runtime registered"`
error to the parent LLM rather than spawning a child. Wiring needs the
actor-spawner closure factored out of `Router::with_actor_spawner` so
`LocalSubagentRuntime` can reuse it.

### `user_id` plumbing through `TraceEvent`

`TraceEvent::LlmSpanEnded` carries no `user_id`, so `cost_records.user_id`
ends up empty and `user_monthly_cost` cache rows aggregate per-month-only
(not per-user-per-month). Fix: thread `user_id` through `SpanRecorder` at
construction time (per-session recorder, per-session user) so every event
carries the owning user.

### `cost_summary(Session)` / `cost_summary(Job)` indices

Both currently return empty (`QueryApi::cost_summary_for` is stubbed).
`CostStore` needs `query_session(session_id)` / `query_job(job_id)` —
single SQL filter on the existing `cost_records` indices.

### `SpanEvent::HookDegraded` audit emission

`AgentLoop::fire_pre_step` / `fire_post_step` currently log a `tracing::warn`
on hook timeout but don't persist a `SpanEvent::HookDegraded`. The blocker:
SpanEvent rows require a `span_id`, and step boundaries don't have one.
Two paths:
- (a) open a "host marker" span on each step purely to anchor cross-cutting
  events
- (b) extend the trace schema with `step_events` keyed by `(step_id, seq)`

### Per-hook timeout config + auto-disable

`AgentLoop` uses a single `STEP_HOOK_TIMEOUT = 500ms` for all PreStep /
PostStep hooks. Q17 calls for per-hook configurable timeout + auto-disable
after N consecutive timeouts. Adding per-hook timeout means extending the
`Hook` trait (or registration path) with a `timeout()` method; auto-disable
needs HookManager state for consecutive-timeout counts.

### Compression-LLM-call for subagent spawn

`SubagentRuntime::spawn` passes `task_description` + `must_include_context`
verbatim into the child's first user message. The design's "background
summary" step (Q10 A3) — running an extra summarizer LLM call on the
parent's recent messages and emitting it as a `StepKind::Compression` step
before `StepKind::Subagent` — is documented but not implemented.

### Wire auxiliary `StepKind`s into the agent loop

Four `StepKind` variants (`Compression`, `MemoryRecall`, `MemoryWrite`,
`SkillSelection`) are defined and serialize correctly, but `agent_loop.rs`
never opens steps for them. Today the corresponding LLM calls happen
silently — e.g. `context_manager.maybe_compress(...)` at
`agent_loop.rs:413` runs an LLM round-trip with no `begin_step` /
`end_step` around it, so the trace tree shows only `LlmIteration` steps
and the compression cost is invisible to per-step cost aggregation.

Each variant needs its real driver to wrap the LLM call in
`begin_step(<variant>)` → `begin_span(LlmCall)` → `end_span` →
`end_step`, matching what `LlmIteration` already does. Drivers:

- `Compression` — `ContextManager::maybe_compress`
- `MemoryRecall` / `MemoryWrite` — the memory subsystem when it lands
- `SkillSelection` — the skill selector when it lands

The reason these are kept as their own steps (not sub-spans of
`LlmIteration`) is that each is an autonomous LLM call with its own
cost / lifecycle / failure mode, fires at a different cadence than
the main loop, and benefits from `PreStep` / `PostStep` hook handlers
being able to discriminate by step kind.

### Multi-job subagent completion signal

`LocalSubagentRuntime` returns on the first `AgentOutput::Message` from the
child. Q10 D2's "child may run multi-job; parent waits for all terminal"
needs a richer completion signal (e.g. `AgentOutput::JobCompleted`) so the
runtime can correctly wait for the entire child job chain.

### CLI / gateway adoption of `QueryApi`

`crates/agent/src/query.rs` is built and unit-tested but not wired into the
gateway admin surface or the CLI commands. The existing `aura-cli/src/commands/trace.rs`
does ad-hoc step-counting that can be replaced by `QueryApi::lineage_tree`
/ `replay`. Same for `gateway/src/api/admin/traces.rs`.

### Retention janitor for auto-growing tables

Several tables grow linearly with system activity but have no
scheduled sweeper. `aura-janitor` is the natural home — one daily
pass over the set below (plus a sub-hourly pass for
`channel_pairings`). Each task needs its own `purge_*_older_than` /
LRU method on the relevant store; only `trace_events` and
`user_monthly_cost` already expose one today.

| Table | Trigger | Default | Notes |
|---|---|---|---|
| `trace_events` | `at < cutoff` (already soft-deleted by `compact_before`) | 30d | Recovery-only WAL; safe to drop once past the recovery horizon. |
| `cost_records` | `timestamp < cutoff` | 90d | Aggregate already lives in `user_monthly_cost`. Per-row detail is high-volume — at 1k jobs/day with ~10 LLM spans each, ~1 GB/year. |
| `cron_executions` | `status='completed' AND triggered_at < cutoff` | 30d | A 1-min cron alone produces ~525k rows/yr. |
| `blobs` | `last_accessed_at < cutoff AND <no live spans/cost_records reference>` | 30d | LRU eviction. The `last_accessed_at` column is already maintained for this purpose. |
| `channel_pairings` | `expires_at < now() OR (status='approved' AND approved_at < cutoff)` | hourly / 7d | Auth-flow ephemera; pairing codes are short-lived by nature. |
| `job_transitions` / `job_verification_transitions` | cascade with hard-purge of the owning job (no independent sweep needed) | — | Bounded per job (~3-5 rows). Don't need a standalone task — fold into whatever drives `jobs` hard-deletes. |

Out of scope (do **not** auto-purge): `steps`, `spans`, `span_events`,
`skill_risk_assessment_jobs`, `user_monthly_cost`. Trace data + monthly
cost rollups are user-facing history; risk assessment job rows are
keyed by content hash and only invalidate when the underlying skill
changes.

### TUI live trace stream

`TraceEventStream` flows in-process. Surfacing it across the gateway WS
protocol to the TUI for live progress display requires a new frame
variant (`Frame::TraceEvent` or similar) plus a TUI render layer. Scoped
to whichever PR adds the live-progress view.

### `aura-trace` doc comment for `outcome.rs::pending_is_not_terminal`

Trivial cosmetic — the test name reads "pending is not terminal", but the
design treats `Pending` as an in-flight state, which the variant naming
already conveys. Rename for clarity if anyone touches the file.
