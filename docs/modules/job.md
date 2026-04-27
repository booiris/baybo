# job - Job Types and State Machine

## Overview

The `job` crate defines domain types for job lifecycle management (`Job`, `JobStatus`, `JobTransition`, `OperationKind`) and the `JobError` error type. `Job` owns the state machine: construction, transition validation, timestamp management, and convenience methods all live on the type itself.

Business logic (`JobManager` — persistence orchestration) lives in `agent::job`. The `JobStore` trait is defined in `storage::job`.

Job answers **"what step is this operation at"**, not "what exactly did it do." Detailed input/output is recorded by `trace`.

## Design Decisions

### Fixed state machine

```
Pending ──► InProgress ──► Completed ──► Submitted ──► Accepted
       │         │              │            \→ Failed
       │         ├─► Failed     │
       │         ├─► Cancelled  │
       │         └─► Stuck ──► InProgress
       │                  ├─► Failed
       │                  ├─► Cancelled
       │                  └─► Abandoned
       └─► Cancelled
```

- **Pending**: created, waiting to execute
- **InProgress**: currently executing
- **Completed**: agent finished; waiting for the acceptance chain to advance
- **Submitted**: agent's output queued for verification
- **Accepted**: successful terminal state (verifier signed off)
- **Failed**: terminal — system error or rejected at `Submitted`
- **Cancelled**: terminal — user explicitly aborted
- **Stuck**: agent hung or interrupted; awaiting recovery decision
- **Abandoned**: terminal — recovery worker gave up on a `Stuck` job

Every transition is validated strictly. Illegal transitions return errors, never silently overwrite.

`Cancelled` is distinct from `Failed` so cost / failure-rate metrics can exclude voluntary aborts. `Abandoned` is distinct from `Failed` so operators can tell "system gave up after retrying" from "system error". `Submitted → Failed` exists for verifier rejection — `completed_at` is preserved across that transition so the audit row reflects when the agent actually stopped, not when the rejection landed.

### Job owns its behavior

`Job` is not a passive data struct — it encapsulates the state machine:

- `Job::new(session_id, kind, parent)` — constructor with UUID, `Pending` status, default `AcceptancePolicy::Auto`, default `RecoveryPolicy::AutoResume { max_attempts: 3 }`
- `Job::transition(target, output, error, reason)` — validates transition, mutates status/timestamps/output/error/recovery_attempts, returns `JobTransition` record
- Convenience methods: `start()`, `complete(output)`, `submit()`, `accept()`, `fail(error)`, `cancel(reason)`, `abandon(reason)`, `stuck(reason)`, `recover(reason)`
- `Job::mark_interrupted()` — for restart recovery; transitions `InProgress → Stuck`, returns `None` for other statuses
- `Job::is_terminal()` — true for `Accepted`, `Failed`, `Cancelled`, or `Abandoned`
- `JobStatus::needs_recovery()` — true for all non-terminal statuses

Timestamp rules are enforced inside `transition()`:
- `started_at` is set on first entry to `InProgress` (not on recovery re-entry)
- `completed_at` is set the first time the job leaves the live phase (any of `Completed`, `Failed`, `Cancelled`, `Abandoned`, or directly `Accepted`); never overwritten on `Submitted → Failed`
- `submitted_at` / `accepted_at` are stamped on the corresponding transitions
- `recovery_attempts` increments on every `Stuck → InProgress` transition

### Acceptance and recovery policies

Two per-job policies drive the post-completion flow:

`AcceptancePolicy` — who walks `Completed → Submitted → Accepted`:
- `Auto` (default) — `JobManager::complete()` walks the whole chain in one in-memory batch then saves once. Chat turns, cron, and system actions take this path so users never see the verifier seam.
- `AutoSubmit { acceptor }` — `complete()` advances to `Submitted` and stops; an external `Acceptor` (user via `POST /v1/jobs/{id}/accept`, validator tool, timeout) flips to `Accepted`.
- `Manual { submitter, acceptor }` — `complete()` stops at `Completed`; both transitions wait for explicit triggers.

`RecoveryPolicy` — what `JobManager::apply_recovery_policy()` does with a `Stuck` job:
- `AutoResume { max_attempts }` (default `max_attempts = 3`) — leave the job `Stuck` if attempts remain (an external resumer drives `Stuck → InProgress`); abandon when the counter hits the limit.
- `Manual` — leave `Stuck` indefinitely; only an operator decides.
- `Abandon` — move to `Abandoned` immediately on the next sweep.

This keeps the state machine invariants co-located with the type and makes them testable without any storage dependency.

### JobManager orchestrates persistence and the policy chains

`JobManager` in `agent::job` is a persistence orchestrator with policy-aware helpers. The `Job` type still owns transition validation; the manager just composes them.

For `complete()` it loads the job, calls `Job::complete` and (per `AcceptancePolicy`) `Job::submit` / `Job::accept` in memory, then writes the final state with a single `store.save()` followed by per-transition records. This makes the `Auto` chain crash-safe: a process exit between `save` and the per-transition records leaves the job in its final state, only losing audit rows.

`JobManager::bootstrap_recovery()` runs at startup in this order:

1. `recover_interrupted` — every `InProgress` job becomes `Stuck` via `Job::mark_interrupted()`.
2. `reconcile_auto_chains` — any `Auto`-policy job stranded at `Completed` or `Submitted` (legacy data, manual store mutation) is forward-filled to `Accepted` in one save.
3. `apply_recovery_policy` — every `Stuck` job has its `RecoveryPolicy` evaluated; `Abandon` and exhausted `AutoResume` move to `Abandoned`, the rest are left for an external resumer.

The chain order matters: step 2 only touches non-`InProgress` states, so step 1 must run first; step 3 only touches `Stuck`, so it must run after step 2 in case a stranded job was actually mid-completion.

### Unified success path

All `Auto`-policy completions reach `Accepted` in a single `complete()` call. `AutoSubmit` and `Manual` jobs deliberately stop earlier and wait for their acceptor — they're audit-trail terminals only after the operator/validator/timeout signs off.

### Stuck and recovery

`Stuck` means execution state is unknown or hung (e.g. LLM timeout, tool stuck). The policy on the `Job` decides what `apply_recovery_policy` does — see the `RecoveryPolicy` description above. Manual operator action via `Job::recover()` is always available regardless of policy.

### Restart recovery

`bootstrap_recovery` (above) runs once before the gateway accepts messages. `Cancelled` and `Abandoned` jobs are terminal and never recover.

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
