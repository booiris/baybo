# job - Job Types and State Machine

## Overview

The `job` crate defines domain types for job lifecycle management (`Job`, `JobStatus`, `JobTransition`, `OperationKind`) and the `JobError` error type. `Job` owns the state machine: construction, transition validation, timestamp management, and convenience methods all live on the type itself.

Business logic (`JobManager` — persistence orchestration) lives in `agent::job`. The `JobStore` trait is defined in `storage::job`.

Job answers **"what step is this operation at"**, not "what exactly did it do." Detailed input/output is recorded by `trace`.

## Design Decisions

### Fixed state machine

```
Pending → InProgress → Completed → Submitted → Accepted
                   \→ Failed
                   \→ Stuck → InProgress
                           \→ Failed
```

- **Pending**: created, waiting to execute
- **InProgress**: currently executing
- **Completed**: finished, waiting for final confirmation chain
- **Submitted**: waiting for final confirmation
- **Accepted**: successful terminal state
- **Failed**: failed terminal state
- **Stuck**: hung, waiting for recovery or failure judgment

Every transition is validated strictly. Illegal transitions return errors, never silently overwrite.

### Job owns its behavior

`Job` is not a passive data struct — it encapsulates the state machine:

- `Job::new(session_id, kind, parent)` — constructor with UUID, `Pending` status, timestamps
- `Job::transition(target, output, error, reason)` — validates transition, mutates status/timestamps/output/error, returns `JobTransition` record
- Convenience methods: `start()`, `complete(output)`, `submit()`, `accept()`, `fail(error)`, `stuck(reason)`, `recover(reason)`
- `Job::mark_interrupted()` — for restart recovery; transitions `InProgress → Stuck`, returns `None` for other statuses
- `Job::is_terminal()` — true for `Accepted` or `Failed`
- `JobStatus::needs_recovery()` — true for all non-terminal statuses

Timestamp rules are enforced inside `transition()`:
- `started_at` is set on first entry to `InProgress` (not on recovery re-entry)
- `completed_at` is set on entry to `Completed`, `Accepted`, or `Failed`

This keeps the state machine invariants co-located with the type and makes them testable without any storage dependency.

### JobManager is a thin persistence orchestrator

`JobManager` in `agent::job` does only: load from store → call `job.transition()` → `store.save()` + `store.record_transition()`. No state machine logic in the manager.

Additionally, `JobManager::recover_interrupted()` handles startup recovery — scan non-terminal jobs and apply `mark_interrupted()` to each. Called once at system startup before accepting messages.

### Unified success path

All successfully completed Jobs follow the same path: `Pending → InProgress → Completed → Submitted → Accepted`. No special cases where some Jobs end at `Completed` — it is always an intermediate state.

### Stuck and recovery

`Stuck` means execution state is unknown or hung (e.g. LLM timeout, WASM tool stuck). Recovery: watchdog scans `InProgress` → timeout → `stuck()` → system decides `recover()` (back to `InProgress`) or `fail()`.

### Restart recovery

On startup, `JobManager::recover_interrupted()` scans all non-terminal jobs. `InProgress` jobs are moved to `Stuck` (via `Job::mark_interrupted()`) because the executing context was lost; other non-terminal states (`Pending`, `Completed`, `Submitted`, `Stuck`) are left unchanged — they can resume without a state change. Upper-layer logic then decides whether to `recover()` or `fail()` each `Stuck` job.

Helper methods on the types:
- `JobStatus::needs_recovery()` — `true` for all non-terminal statuses
- `Job::mark_interrupted()` — transitions `InProgress → Stuck` with reason, returns `None` for other statuses

### Job hierarchy

Jobs support parent-child relationships via `parent_job_id`. Child success/failure does not auto-rewrite parent state — that's upper-layer business logic. `list_children()` returns only direct children.

### Collaboration with Trace

| Dimension | Job | Trace |
|-----------|-----|-------|
| Focus | State | Content |
| Key fields | `status`, timestamps, hierarchy | `input`, `result`, `latency`, `provenance` |
| Sensitive data | Sanitized JSON only | Sanitized payloads/summaries only |

`ObservabilityRecorder` calls both simultaneously, cross-linked via `job_id` and `trace_span_id`.

## Constraints

- Pure types crate — no storage interfaces, no async
- `input/output` store sanitized JSON only — sensitive values must already be placeholders
- `save()` and `record_transition()` should run in the same transaction (enforced by `JobManager`)
- Does not depend on `trace`, `llm`, `tools`, or `agent`

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `agent::job::JobManager` owns persistence logic; `ObservabilityRecorder` creates and transitions Jobs |
| `trace` | Linked via `trace_span_id` for content details |
| `hook` | `JobStatusChanged` fires after state changes |
| `storage` | Defines `JobStore` trait using job types; provides libsql implementation |
