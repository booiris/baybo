# job - Job Types and State Machine

## Overview

The `job` crate defines domain types for job lifecycle management (`Job`, `JobStatus`, `JobKind`, `JobInput`, `JobOutput`, `CancelReason`, `JobTransition`) and the `JobError` error type. `Job` owns the state machine: construction, transition validation, timestamp management, and convenience methods all live on the type itself.

Business logic (`JobLifecycle` — persistence orchestration and hook invocation) lives in `agent::job`. The `JobStore` trait is defined in `storage::job`.

Job answers **"what step is this operation at"**, not "what exactly did it do." Detailed input/output is recorded by `trace`. Each job carries its own `final_result` for the final contractual value, but progress messages emitted mid-job live in the trace tree — `Job.emitted_span_ids` is an index, not a copy.

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

`Cancelled` carries a `reason: CancelReason` (`UserPreempt`, `SystemCrash`, `SubagentTimeout`, `ParentCancelled`, `ParentDeleted`, `HookAborted`) and `partial_artifacts: Vec<SpanId>` — the spans that completed (or partially completed) before the cancel. The next job's prompt-assembly step reads these spans and renders a "previously completed steps:" preamble so the LLM has context. Content lives only in the trace; `Job.partial_artifacts` is indices.

`Cancelled` is reused for crash recovery — the recovery scan rewrites half-open spans with `reason: SystemCrash` and rolls them up into the parent job's `partial_artifacts`.

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

- `Job::new(session_id, kind, input, parent)` — constructor with ULID, `Pending` status, timestamps
- `Job::transition(target, ...)` — validates transition, mutates status/timestamps, returns `JobTransition` record
- Convenience methods: `start()`, `complete(output)`, `fail(reason)`, `cancel(reason, partial_artifacts)`, `stuck(reason)`, `recover()`
- `Job::mark_interrupted()` — for restart recovery; transitions `InProgress → Stuck { reason: "system_crash" }`, returns `None` for other statuses
- `Job::is_terminal()` — true for `Completed | Cancelled | Failed`
- `JobStatus::needs_recovery()` — true for `Pending | InProgress | Stuck`

Timestamp rules are enforced inside `transition()`:
- `started_at` is set on first entry to `InProgress` (not on recovery re-entry)
- `ended_at` is set on entry to any terminal state (`Completed`, `Cancelled`, `Failed`)

This keeps the state machine invariants co-located with the type and makes them testable without any storage dependency.

### JobLifecycle is a thin persistence + hook orchestrator

`JobLifecycle` in `agent::job` does only: load from store → call `job.transition()` → `store.save()` + `store.record_transition()` → fire `PreStep` / `PostStep` hooks at step boundaries (with the timeout / degraded protocol — see `agent.md`). No state machine logic in the orchestrator.

Additionally, `JobLifecycle::recover_interrupted()` handles startup recovery — scan non-terminal jobs and apply `mark_interrupted()` to each. Called once at system startup before accepting messages.

### Restart recovery

On startup, `JobLifecycle::recover_interrupted()` scans all non-terminal jobs. `InProgress` jobs are moved to `Stuck` (via `Job::mark_interrupted()`) because the executing context was lost; other non-terminal states (`Pending`, `Stuck`) are left unchanged — they can resume without a state change. Half-open spans under those jobs are rewritten by the trace recovery scan to `Cancelled { SystemCrash }` and rolled into `partial_artifacts`. Upper-layer logic then decides whether to `recover()` or `fail()` each `Stuck` job.

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
- `input` / `final_result` / `partial_artifacts` store sanitized JSON / span-id lists only — sensitive values must already be placeholders
- `save()` and `record_transition()` should run in the same transaction (enforced by `JobLifecycle`)
- `session.trigger.kind() == job.kind()` invariant is enforced at `Job::new`
- Does not depend on `trace`, `llm`, `tools`, or `agent`

## Collaboration

| Module    | Role                                                                                                      |
| --------- | --------------------------------------------------------------------------------------------------------- |
| `agent`   | `agent::job::JobLifecycle` owns persistence + hook invocation; replaces the legacy `ObservabilityRecorder` |
| `trace`   | Provides `SpanId`; `partial_artifacts` references trace spans; recovery coordinates with the trace scan    |
| `hook`    | `JobStatusChanged`, `PreStep`, `PostStep` fire from `JobLifecycle`                                         |
| `storage` | Defines `JobStore` trait using job types; provides libsql implementation                                   |
| `session` | `Session.trigger.kind() == Job.kind()` invariant; `Lineage` consumes `parent_job_id`                       |
