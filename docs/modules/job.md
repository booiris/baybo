# job - Job Types, Store, and State Machine

## Overview

The `job` crate is the home for the Job concept: domain types (`Job`, `JobStatus`, `JobInputKind`, `JobInput`, `JobOutput`, `CancelReason`, `JobTransition`, `JobError`), the row conversions that persist them, and the `JobLifecycle` persistence orchestrator. `Job` owns the state machine: construction, transition validation, timestamp management, and convenience methods all live on the type itself; `JobLifecycle` wraps the `JobStore` with the cancel state machine, lifecycle-event bus, and `JobId → CancellationToken` registry the in-flight execution path subscribes to.

The `JobStore` trait itself lives in the `baybo-store` ports crate and trades in row DTOs — `JobRow` (the queryable columns plus the serialized `Job` in `data`) and `JobTransitionRow`. This crate owns the `Job::to_row` / `Job::from_row` conversions, so the state machine stays here while the trait sits in a leaf crate every store consumer can reach. `baybo-storage` provides the sqlite implementation, shuttling rows without depending on `baybo-job` (it converts in its tests only). `impl From<baybo_store::StorageError> for JobError` bridges errors at the call sites.

Job answers **"what step is this operation at"**, not "what exactly did it do." Detailed input/output is recorded by `trace`. Each job carries its own `final_result` for the final contractual value, but progress messages emitted mid-job live in the trace tree — `Job.emitted_span_ids` is an index, not a copy. Spans completed before a cancel are tracked separately on `JobStatus::Cancelled { reason, partial_artifacts }`, not as a top-level `Job` field.

## Design Decisions

### State machine

```
Pending → InProgress → Completed
      |            \→ Stuck { reason } → InProgress
      |                               \→ Failed { reason }
      |                               \→ Cancelled { reason, partial_artifacts }
      |            \→ Failed { reason }
      |            \→ Cancelled { reason, partial_artifacts }
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

`Cancelled` carries a `reason: CancelReason` (`UserPreempt`, `SystemCrash`, `SubagentTimeout`, `ParentCancelled`, `ParentDeleted`, `OperatorCancel`, `UserStopped`) and `partial_artifacts: Vec<SpanId>` — the spans that completed (or partially completed) before the cancel. Both fields are nested **inside** the `JobStatus::Cancelled { reason, partial_artifacts }` variant; the top-level `Job` exposes `emitted_span_ids` for general progress indexing. The field is reserved for a future prompt-assembly preamble that would surface those spans to the next job's LLM; no consumer reads it today, and every production cancel path currently passes an empty list. Content lives only in the trace; the field is indices.

`SystemCrash` is used when Baybo owns the cleanup after execution disappeared:
the boot recovery sweep rolls jobs left non-terminal by a prior process death to
`Cancelled { SystemCrash }`, and the in-process actor panic runner does the same
for the panicked session's active turn jobs.

### Two orthogonal descriptors: input kind and origin

A job is described along two independent axes, each with one source of truth — replacing a single overloaded `kind` that conflated "what payload" with "which trigger":

```rust
// input kind — what payload fed the job; a projection of JobInput.
// Display / the denormalised `jobs.kind` column only.
pub enum JobInputKind { UserChat, Cron, CronNotification, Compact, Spawned, SubagentNotification }
```

- **input kind** (`JobInputKind`) — `Job::input_kind()`, projected from `JobInput`. `JobInput` is a strongly typed payload enum whose variants line up 1:1 with `JobInputKind`. `UserChat`, `Cron`, `Spawned`, and `SubagentNotification` are turn jobs. `Job::is_turn()` excludes two: `Compact` (a foreground maintenance command, no reply) and `CronNotification` (the delivery of a one-shot cron fire's result into the conversation that scheduled it — it appends a reply the fire already produced, running no inference, so there is nothing in flight for a user to wait on or for `/stop` to interrupt). `CronNotification` still opens a real job: its `Completed { reply_ordinal }` edge is what drives the push dispatcher off the row it just appended, exactly as a user turn does.
- **origin** (`baybo_model::TriggerKind`, stored on `Job.origin`) — the owning session's root trigger, recorded **as-is** at creation. It is *not* asserted against the payload: `/compact` can run inside a `User`-trigger session while carrying a `Compact` input. Subagent jobs record `origin = Spawned` (their session's inherited root).

`JobOutput` does not split this way — it has only `Message` and `Structured`, the two shapes any job can produce.

### Job owns its behavior

`Job` is not a passive data struct — it encapsulates the state machine:

- `Job::new(session_id, origin, input, parent_job_id)` — constructor with ULID, `Pending` status, timestamps. `origin` is supplied by the caller; `input_kind` is projected from `input`.
- `Job::transition(target, ...)` — validates transition, mutates status/timestamps, returns a `JobTransition` legality receipt (consumed and dropped by `JobLifecycle`; the per-transition audit table was retired in the 2026-07 unused-column audit). `transition_at(target, ..., at)` / `cancel_at(reason, artifacts, at)` are explicit-timestamp variants reserved for the boot-recovery sweep, which must backdate `ended_at` to the last observed activity rather than the boot wall-clock; live callers use `transition` / `cancel`.
- Convenience methods: `start()`, `complete(output)`, `fail(reason)`, `cancel(reason, partial_artifacts)`, `stuck(reason)`, `recover(reason)`
- `Job::is_terminal()` — true for `Completed | Cancelled | Failed`
- `JobStatus::needs_recovery()` — true for `Pending | InProgress | Stuck` (consumed by admin queries that surface in-flight jobs)

Timestamp rules are enforced inside `transition()`:
- `started_at` is set on first entry to `InProgress` (not on recovery re-entry)
- `ended_at` is set on entry to any terminal state (`Completed`, `Cancelled`, `Failed`)

This keeps the state machine invariants co-located with the type and makes them testable without any storage dependency.

### JobLifecycle is a thin persistence orchestrator

`JobLifecycle` does only: load from store → call `job.transition()` → `store.save()`. No state machine logic in the orchestrator. It additionally owns:

- A `tokio::sync::broadcast` bus that publishes a `JobLifecycleEvent` (id, session, parent, phase, input kind; the `Completed` phase additionally carries the reply's persisted `session_messages.ordinal`) on `Pending → InProgress` and on every `Completed | Failed | Cancelled` transition. Subscribers: the subagent runtime waits for terminal phases, the TurnState projection treats every phase as a recompute trigger, and the push dispatcher filters `Completed` events to the kinds a user is meant to read (`UserChat`, `Cron` — confirmed against the session's `conversation` marker — and `CronNotification`). Lagging subscribers must reconcile via store reads such as `list_by_session` / `active_turn_started_at` — a dropped event is not re-published.
- A `JobCancellationRegistry` mapping `JobId → CancellationToken` for in-flight jobs. `JobLifecycle::cancel` trips the registered token *before* flipping the row, so the running execution observes the cancel before terminal-state observers do. `register_running` returns a RAII `JobCancellationGuard` that unregisters on drop, so an early `?` from the agent loop can't leak entries.

### Recovery

The state-machine and storage shape support recovery of non-terminal jobs
(`Pending` / `InProgress` / `Stuck`). `baybo_agent::recovery` owns the cross-table
repair because it has both job and trace stores:

- Boot recovery scans all non-terminal jobs from the prior process, closes any
  half-open trace rows at the last observed activity time, and calls
  `JobLifecycle::cancel_at(..., SystemCrash, ...)`.
- Actor panic recovery scans only the panicked session's active turn jobs,
  closes their half-open trace rows at the actor crash time, and cancels them as
  `SystemCrash`.

`JobStatus::Cancelled.partial_artifacts` remains the resume hook for spans that
completed before cancellation; content itself lives in trace.

### Job hierarchy

Jobs support parent-child relationships via `parent_job_id`. Child success/failure does not auto-rewrite parent state — that's upper-layer business logic. `list_children()` returns only direct children.

### Per-trigger queue / preempt policy

The job state machine itself is trigger-agnostic, but the actor that drives it follows per-trigger policy. The actor is a serial loop over a priority mailbox, so nothing a *trigger* carries preempts a running turn — only the subagent token tree does:

| Session trigger | New trigger arriving while a job is `InProgress`                |
| --------------- | --------------------------------------------------------------- |
| `User`          | Queue / inject: lands in the actor mailbox at `Trigger` priority (see below) |
| `Cron`          | Queue: actor mailbox holds it until current job is terminal     |
| Subagent (any)  | Preempt: parent's cancellation token tree propagates downward   |

A running turn drains the leading run of non-slash user inputs at each tool boundary and injects them mid-turn (non-preemptive — never mid-LLM-call); anything still queued when the turn ends is coalesced into the next turn. Preemption is not implemented — `CancelReason::UserPreempt` has no production producer today; the out-of-band `/stop` is the only way to cancel a running turn.

`/stop` cancels the in-flight turn (and every in-flight descendant subagent) with `Cancelled { UserStopped, ... }` — suppression of the terminal `BackgroundJobFinished` delivery comes from `/stop` draining the supervisor's in-flight background-subagent registry (each child's wait task finds its entry gone and drops the delivery), so a stopped result never repopulates the parent notification buffer; `UserStopped` is the audit reason stamped on the cancelled rows.

### Collaboration with Trace

| Dimension      | Job                                                  | Trace                                                       |
| -------------- | ---------------------------------------------------- | ----------------------------------------------------------- |
| Focus          | State                                                | Content                                                     |
| Key fields     | `status`, timestamps, hierarchy, `final_result`      | `step_id`, `span_id`, kind-specific input/output/provenance |
| Sensitive data | Sanitized JSON only                                  | Sanitized payloads/summaries only                           |

`JobLifecycle` lives in this crate; `SpanRecorder` (in `baybo-trace`) is its peer facade. They do not share a transaction; cross-table consistency is reconciled by the recovery scan (per-table transactions, eventually consistent).

## Constraints

- `input` / `final_result` / `JobStatus::Cancelled.partial_artifacts` store sanitized JSON / span-id lists only — sensitive values must already be placeholders
- `Job.origin` is supplied by the caller at `JobLifecycle::start_job` (via `JobSpec.origin`) and passed straight into `Job::new`; it is not validated against the payload. Only `input_kind` is projected from `input`
- Does not depend on `trace`, `llm`, `tools`, or `agent`. Depends only on `baybo-model` (IDs) and `baybo-store` (the `JobStore` trait + row DTOs).
- `test_support::MemoryJobStore` is gated behind the `test-support` feature so it never ships in release builds. Downstream test crates pull it in via `baybo-job = { workspace = true, features = ["test-support"] }`.

## Collaboration

| Module    | Role                                                                                                      |
| --------- | --------------------------------------------------------------------------------------------------------- |
| `agent`   | Consumes `JobLifecycle` to drive jobs through the agent loop; supplies the cancellation tokens that `register_running` tracks |
| `trace`   | Provides `SpanId`; `JobStatus::Cancelled.partial_artifacts` references trace spans; recovery coordinates with the trace scan    |
| `store`   | Owns the `JobStore` trait + its `JobRow` / `JobTransitionRow` DTOs and `StorageError`; this crate converts `Job` ↔ rows |
| `storage` | Provides the sqlite implementation of `JobStore` (from `baybo-store`), shuttling rows; depends on `baybo-job` only as a dev-dependency |
| `session` | `Session.trigger.kind()` is recorded as `Job.origin`; `Lineage` consumes `parent_job_id`                       |
