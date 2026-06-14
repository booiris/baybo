# job - Job Types, Store, and State Machine

## Overview

The `job` crate is the home for the Job concept: domain types (`Job`, `JobStatus`, `JobInputKind`, `JobShape`, `JobInput`, `JobOutput`, `CancelReason`, `JobTransition`, `JobError`), the row conversions that persist them, and the `JobLifecycle` persistence orchestrator. `Job` owns the state machine: construction, transition validation, timestamp management, and convenience methods all live on the type itself; `JobLifecycle` wraps the `JobStore` with the cancel state machine, terminal-event bus, and `JobId → CancellationToken` registry the in-flight execution path subscribes to.

The `JobStore` trait itself lives in the `aura-store` ports crate and trades in row DTOs — `JobRow` (the queryable columns plus the serialized `Job` in `data`) and `JobTransitionRow`. This crate owns the `Job::to_row` / `Job::from_row` conversions, so the state machine stays here while the trait sits in a leaf crate every store consumer can reach. `aura-storage` provides the libsql implementation, shuttling rows without depending on `aura-job` (it converts in its tests only). `impl From<aura_store::StorageError> for JobError` bridges errors at the call sites.

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

`Cancelled` carries a `reason: CancelReason` (`UserPreempt`, `SystemCrash`, `SubagentTimeout`, `ParentCancelled`, `ParentDeleted`, `OperatorCancel`, `UserStopped`) and `partial_artifacts: Vec<SpanId>` — the spans that completed (or partially completed) before the cancel. Both fields are nested **inside** the `JobStatus::Cancelled { reason, partial_artifacts }` variant; the top-level `Job` exposes `emitted_span_ids` for general progress indexing. The next job's prompt-assembly step reads `partial_artifacts` and renders a "previously completed steps:" preamble so the LLM has context. Content lives only in the trace; the field is indices.

`SystemCrash` is reserved for a future restart-recovery scan — there is no production code path that mints it today.

### Three orthogonal descriptors: input kind, origin, shape

A job is described along three independent axes, each with one source of truth — replacing a single overloaded `kind` that conflated "what payload" with "which trigger":

```rust
// input kind — what payload fed the job; a projection of JobInput.
// Display / the denormalised `jobs.kind` column only.
pub enum JobInputKind { UserChat, Cron, System, Spawned, SubagentNotification }

// shape — does it run a full agent-loop turn, or a one-shot
// maintenance pass? Declared by the spawning code path.
pub enum JobShape { Turn, Maintenance }
```

- **input kind** (`JobInputKind`) — `Job::input_kind()`, projected from `JobInput`. `JobInput` is a strongly typed payload enum whose variants line up 1:1 with `JobInputKind`.
- **origin** (`aura_model::TriggerKind`, stored on `Job.origin`) — the owning session's root trigger, recorded **as-is** at creation. It is *not* asserted against the payload: background compression runs inside a `User`-trigger session and records `origin = User` while carrying a `System` input. Subagent jobs record `origin = Spawned` (their session's inherited root).
- **shape** (`JobShape`, stored on `Job.shape`) — **declared by the code path that runs the job**, not inferred from the payload: `run()` mints `Turn`; both background compression and the foreground `/compact` mint `Maintenance` (the latter despite its `UserChat` input — inferring shape from the input would mislabel it a turn). `Job::is_turn()` reads it. A session serialises its turns (≤1 active turn-job) but may run a concurrent `Maintenance` job, which is why `list_active_by_session` returns a `Vec`.

`JobOutput` does not split this way — it has only `Message` and `Structured`, the two shapes any job can produce.

### Job owns its behavior

`Job` is not a passive data struct — it encapsulates the state machine:

- `Job::new(session_id, origin, shape, input, parent_job_id)` — constructor with ULID, `Pending` status, timestamps. `origin` and `shape` are supplied by the caller; `input_kind` is projected from `input`.
- `Job::transition(target, ...)` — validates transition, mutates status/timestamps, returns `JobTransition` record. `transition_at(target, ..., at)` / `cancel_at(reason, artifacts, at)` are explicit-timestamp variants reserved for the boot-recovery sweep, which must backdate `ended_at` to the last observed activity rather than the boot wall-clock; live callers use `transition` / `cancel`.
- Convenience methods: `start()`, `complete(output)`, `fail(reason)`, `cancel(reason, partial_artifacts)`, `stuck(reason)`, `recover()`
- `Job::is_terminal()` — true for `Completed | Cancelled | Failed`
- `JobStatus::needs_recovery()` — true for `Pending | InProgress | Stuck` (consumed by admin queries that surface in-flight jobs)

Timestamp rules are enforced inside `transition()`:
- `started_at` is set on first entry to `InProgress` (not on recovery re-entry)
- `ended_at` is set on entry to any terminal state (`Completed`, `Cancelled`, `Failed`)

This keeps the state machine invariants co-located with the type and makes them testable without any storage dependency.

### JobLifecycle is a thin persistence orchestrator

`JobLifecycle` does only: load from store → call `job.transition()` → `store.save()` + `store.record_transition()`. No state machine logic in the orchestrator. It additionally owns:

- A `tokio::sync::broadcast` bus that publishes a `JobTerminalEvent` (id, session, parent, terminal kind) on every `Completed | Failed | Cancelled` transition. Subscribers (subagent runtime, admin UI) wait on this without polling the store. Lagging subscribers must reconcile via `list_by_session` — a dropped event is not re-published.
- A `JobCancellationRegistry` mapping `JobId → CancellationToken` for in-flight jobs. `JobLifecycle::cancel` trips the registered token *before* flipping the row, so the running execution observes the cancel before terminal-state observers do. `register_running` returns a RAII `JobCancellationGuard` that unregisters on drop, so an early `?` from the agent loop can't leak entries.

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

Distinct from a new trigger arriving, the out-of-band `/stop` control command cancels the in-flight turn (and every in-flight descendant subagent) with `Cancelled { UserStopped, ... }` — that reason lets the subagent wait task suppress the terminal `SubagentFinished` delivery so a stopped result never repopulates `pending_subagent_results`.

### Collaboration with Trace

| Dimension      | Job                                                  | Trace                                                       |
| -------------- | ---------------------------------------------------- | ----------------------------------------------------------- |
| Focus          | State                                                | Content                                                     |
| Key fields     | `status`, timestamps, hierarchy, `final_result`      | `step_id`, `span_id`, kind-specific input/output/provenance |
| Sensitive data | Sanitized JSON only                                  | Sanitized payloads/summaries only                           |

`JobLifecycle` lives in this crate; `SpanRecorder` (still in `agent`, pending its own extraction to `aura-trace`) is its peer facade. They do not share a transaction; cross-table consistency is reconciled by the recovery scan (per-table transactions, eventually consistent).

## Constraints

- `input` / `final_result` / `JobStatus::Cancelled.partial_artifacts` store sanitized JSON / span-id lists only — sensitive values must already be placeholders
- `save()` and `record_transition()` should run in the same transaction (enforced by `JobLifecycle`)
- `Job.origin` is recorded from the owning session's trigger at `JobLifecycle::start_job` (passed via `JobSpec.origin`); it is not validated against the payload — input kind and origin are independent. `Job::new` is the type-safe constructor that derives `shape` from `input`
- Does not depend on `trace`, `llm`, `tools`, or `agent`. Depends only on `aura-model` for IDs.
- `test_support::MemoryJobStore` is gated behind the `test-support` feature so it never ships in release builds. Downstream test crates pull it in via `aura-job = { workspace = true, features = ["test-support"] }`.

## Collaboration

| Module    | Role                                                                                                      |
| --------- | --------------------------------------------------------------------------------------------------------- |
| `agent`   | Consumes `JobLifecycle` to drive jobs through the agent loop; supplies the cancellation tokens that `register_running` tracks |
| `trace`   | Provides `SpanId`; `JobStatus::Cancelled.partial_artifacts` references trace spans; recovery coordinates with the trace scan    |
| `store`   | Owns the `JobStore` trait + its `JobRow` / `JobTransitionRow` DTOs and `StorageError`; this crate converts `Job` ↔ rows |
| `storage` | Provides the libsql implementation of `JobStore` (from `aura-store`), shuttling rows; depends on `aura-job` only as a dev-dependency |
| `session` | `Session.trigger.kind()` is recorded as `Job.origin`; `Lineage` consumes `parent_job_id`                       |
