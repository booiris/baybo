# job - Job Types and State Machine

## Overview

The `job` crate defines domain types for job lifecycle management (`Job`, `JobStatus`, `JobKind`, `JobInput`, `JobOutput`, `CancelReason`, `JobTransition`) and the `JobError` error type. `Job` owns the state machine: construction, transition validation, timestamp management, and convenience methods all live on the type itself.

Business logic (`JobLifecycle` — persistence orchestration) lives in `agent::job`. The `JobStore` trait is defined in `storage::job`.

Job answers **"what step is this operation at"**, not "what exactly did it do." Detailed input/output is recorded by `trace`. Each job carries its own `final_result` for the final contractual value, but progress messages emitted mid-job live in the trace tree — `Job.emitted_span_ids` is an index, not a copy. Spans completed before a cancel are tracked separately on `JobStatus::Cancelled { reason, partial_artifacts }`, not as a top-level `Job` field.

## Design Decisions

### State machine

```
Pending → InProgress → Completed
                   \→ Stuck { reason } → InProgress
                                      \→ Failed { reason }
                                      \→ Cancelled { reason, partial_artifacts }
                   \→ Failed { reason }
                   \→ Cancelled { reason, partial_artifacts }
```

- **Pending**: created, waiting to execute
- **InProgress**: currently executing
- **Stuck { reason }**: hung or unknown state, awaiting recovery decision (non-terminal)
- **Cancelled { reason, partial_artifacts }**: stopped before completion (terminal)
- **Failed { reason }**: errored (terminal)
- **Completed**: agent finished its work (terminal)

Every transition is validated strictly. Illegal transitions return errors, never silently overwrite.

### Cancelled is independent of Failed

`Cancelled` carries a `reason: CancelReason` (`UserPreempt`, `SystemCrash`, `SubagentTimeout`, `ParentCancelled`, `ParentDeleted`, `OperatorCancel`) and `partial_artifacts: Vec<SpanId>` — the spans that completed (or partially completed) before the cancel. Both fields are nested **inside** the `JobStatus::Cancelled { reason, partial_artifacts }` variant; the top-level `Job` exposes `emitted_span_ids` for general progress indexing. The next job's prompt-assembly step reads `partial_artifacts` and renders a "previously completed steps:" preamble so the LLM has context. Content lives only in the trace; the field is indices.

`SystemCrash` is reserved for a future restart-recovery scan — there is no production code path that mints it today.

### Job kind mirrors session trigger

```rust
pub enum JobKind {
    UserChat,
    Cron,
    System,
    Spawned,
}
```

Invariant: `session.trigger.kind() == job.kind()` at job creation time. `JobInput` and `JobOutput` are strongly typed enums whose variants line up 1:1 with `JobKind`.

### Job owns its behavior

`Job` is not a passive data struct — it encapsulates the state machine:

- `Job::new(session_id, input, effective_soul_version, parent_job_id)` — constructor with ULID, `Pending` status, timestamps. `kind` is derived from `input.kind()`.
- `Job::transition(target, ...)` — validates transition, mutates status/timestamps, returns `JobTransition` record
- Convenience methods: `start()`, `complete(output)`, `fail(reason)`, `cancel(reason, partial_artifacts)`, `stuck(reason)`, `recover()`
- `Job::is_terminal()` — true for `Completed | Cancelled | Failed`
- `JobStatus::needs_recovery()` — true for `Pending | InProgress | Stuck` (consumed by admin queries that surface in-flight jobs)

Timestamp rules are enforced inside `transition()`:
- `started_at` is set on first entry to `InProgress` (not on recovery re-entry)
- `ended_at` is set on entry to any terminal state (`Completed`, `Cancelled`, `Failed`)

This keeps the state machine invariants co-located with the type and makes them testable without any storage dependency.

### JobLifecycle is a thin persistence orchestrator

`JobLifecycle` in `agent::job` does only: load from store → call `job.transition()` → `store.save()` + `store.record_transition()`. No state machine logic in the orchestrator.

### Restart recovery

Not implemented yet. The state-machine and storage shape leave room for it (`Stuck` is non-terminal; `JobStatus::Cancelled.partial_artifacts` indexes spans that the next job should preamble-render), but there is currently no production code path that scans non-terminal jobs at startup or rewrites half-open spans. A crash leaves jobs and spans in their last-persisted state until an operator cancels them via the admin API.

### Job hierarchy

Jobs support parent-child relationships via `parent_job_id`. Child success/failure does not auto-rewrite parent state — that's upper-layer business logic. `list_children()` returns only direct children.

### Per-trigger queue / preempt policy

The job state machine itself is trigger-agnostic, but the actor that drives it follows per-trigger policy:

| Session trigger | New trigger arriving while a job is `InProgress`                |
| --------------- | --------------------------------------------------------------- |
| `User`          | Preempt: current job → `Cancelled { UserPreempt, ... }`          |
| `Cron`          | Queue: actor mailbox holds it until current job is terminal     |
| `System`        | Queue                                                            |
| Subagent (any)  | Preempt: parent's cancellation token tree propagates downward   |

### Collaboration with Trace

| Dimension      | Job                                                  | Trace                                                       |
| -------------- | ---------------------------------------------------- | ----------------------------------------------------------- |
| Focus          | State                                                | Content                                                     |
| Key fields     | `status`, timestamps, hierarchy, `final_result`      | `step_id`, `span_id`, kind-specific input/output/provenance |
| Sensitive data | Sanitized JSON only                                  | Sanitized payloads/summaries only                           |

`JobLifecycle` and `SpanRecorder` (in `agent`) are separate facades and do not share a transaction; cross-table consistency is reconciled by the recovery scan (per-table transactions, eventually consistent).

## Constraints

- Pure types crate — no storage interfaces, no async
- `input` / `final_result` / `JobStatus::Cancelled.partial_artifacts` store sanitized JSON / span-id lists only — sensitive values must already be placeholders
- `save()` and `record_transition()` should run in the same transaction (enforced by `JobLifecycle`)
- `session.trigger.kind() == job.kind()` invariant is enforced at `JobLifecycle::start_job` (returns `JobError::KindMismatch` on violation); `Job::new` is the type-safe constructor and trusts the caller to have matched kinds upstream
- Does not depend on `trace`, `llm`, `tools`, or `agent`

## Collaboration

| Module    | Role                                                                                                      |
| --------- | --------------------------------------------------------------------------------------------------------- |
| `agent`   | `agent::job::JobLifecycle` owns persistence and the lifecycle state machine                               |
| `trace`   | Provides `SpanId`; `JobStatus::Cancelled.partial_artifacts` references trace spans; recovery coordinates with the trace scan    |
| `storage` | Defines `JobStore` trait using job types; provides libsql implementation                                   |
| `session` | `Session.trigger.kind() == Job.kind()` invariant; `Lineage` consumes `parent_job_id`                       |
