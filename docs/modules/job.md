# job - Job Types and State Machine

## Overview

The `job` crate defines domain types for job lifecycle management (`Job`, `JobStatus`, `JobTransition`, `OperationKind`) and the `JobError` error type. `JobStatus` implements the fixed state machine with strict transition validation.

Business logic (`JobManager` — create, transition, load) lives in `agent::job`. The `JobStore` trait is defined in `storage::job`.

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

### Unified success path

All successfully completed Jobs follow the same path: `Pending → InProgress → Completed → Submitted → Accepted`. No special cases where some Jobs end at `Completed` — it is always an intermediate state.

### Stuck and recovery

`Stuck` means execution state is unknown or hung (e.g. LLM timeout, WASM tool stuck). Recovery: watchdog scans `InProgress` → timeout → `stuck()` → system decides `recover()` (back to `InProgress`) or `fail()`.

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

- Pure types crate — no business logic, no storage interfaces
- `input/output` store sanitized JSON only — sensitive values must already be placeholders
- `update_status()` and `record_transition()` should run in the same transaction
- Does not depend on `trace`, `llm`, `tools`, or `agent`

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `agent::job::JobManager` owns lifecycle logic; `ObservabilityRecorder` creates and transitions Jobs |
| `trace` | Linked via `trace_span_id` for content details |
| `hook` | `JobStatusChanged` fires after state changes |
| `storage` | Defines `JobStore` trait using job types; provides libsql implementation |
