# turn - Turn Types, Store, and State Machine

## Overview

The `turn` crate (`baybo-turn`, `crates/turn`) is the home for the Turn concept: domain types (`Turn`, `TurnStatus`, `TurnInputKind`, `TurnInput`, `TurnOutput`, `CancelReason`, `TurnTransition`, `TurnError`), the row conversions that persist them, and the `TurnLifecycle` persistence orchestrator. `Turn` owns the state machine: construction, transition validation, timestamp management, and convenience methods all live on the type itself; `TurnLifecycle` wraps the `TurnStore` with the cancel state machine, lifecycle-event bus, and `TurnId → CancellationToken` registry the in-flight execution path subscribes to.

The `TurnStore` trait itself lives in the `baybo-store` ports crate and trades in row DTOs — `TurnRow` (the queryable columns plus the serialized `Turn` in `data`). This crate owns the `Turn::to_row` / `Turn::from_row` conversions, so the state machine stays here while the trait sits in a leaf crate every store consumer can reach. `baybo-storage` provides the sqlite implementation over the `turns` table, shuttling rows without depending on `baybo-turn` (it converts in its tests only). `impl From<baybo_store::StorageError> for TurnError` bridges errors at the call sites.

Turn answers **"what step is this operation at"**, not "what exactly did it do." Detailed input/output is recorded by `trace`. Each turn carries its own `final_result` for the final contractual value, but progress messages emitted mid-turn live in the trace tree — `Turn.emitted_span_ids` is an index, not a copy. Spans completed before a cancel are tracked separately on `TurnStatus::Cancelled { reason, partial_artifacts }`, not as a top-level `Turn` field.

## Design Decisions

### A Turn is the row; a chat turn is the subset the user sees

These are two different things, and the docs use the two names deliberately:

- A **Turn** is the row in `turns`. Every externally-triggered unit of work a
  session performs opens one — **including** `/compact` and cron-result
  delivery. If it has a state machine and a trace subtree, it is a Turn.
- A **chat turn** is the subset a user experiences as "the agent is working on
  my message": what drives the `TurnState` projection, what `/stop` cancels,
  what per-session crash recovery closes. `Turn::is_chat_turn()` is the **only**
  predicate for that subset — `TurnLifecycle::list_active_chat_turns_by_session`
  centralizes it so `/stop`, recovery, and the projector cannot each re-derive
  "active reply" from `input_kind` and drift apart.

`is_chat_turn()` is **not** "produces a reply", and it is **not** "drives push":

| `TurnInputKind`        | chat turn (`is_chat_turn`) | appends a reply row                 | drives push |
| ---------------------- | -------------------------- | ----------------------------------- | ----------- |
| `UserChat`             | yes                        | yes                                 | yes         |
| `Cron`                 | yes                        | yes                                 | yes (recurring only — confirmed against the session's `conversation` marker) |
| `Spawned`              | yes                        | yes (in the subagent's own session) | no          |
| `SubagentNotification` | yes                        | yes                                 | no          |
| `CronNotification`     | no                         | yes                                 | yes         |
| `Compact`              | no                         | no                                  | no          |

- **Not "produces a reply".** `CronNotification` appends a durable assistant row
  into the conversation that scheduled the one-shot — a real, user-visible
  reply — and is still excluded, because it **runs no inference**: the reply was
  already produced by the fire, so there is nothing in flight for a user to wait
  on, nothing for `/stop` to interrupt, and nothing for crash recovery to close
  as an abandoned reply.
- **Not "drives push".** The push dispatcher's set is `UserChat` + `Cron` +
  `CronNotification` — it *includes* `CronNotification` (that appended row is
  exactly what the user should be notified about) and *excludes* `Spawned` /
  `SubagentNotification`.

**The three subsets cross; they do not nest.** `Spawned` is a chat turn that
never pushes; `CronNotification` pushes without being a chat turn; `Compact` is
the only kind in none of the three. Reading one column off another — or
open-coding an `input_kind` match where the chat-turn subset was meant — is the
bug this section exists to prevent.

`TurnInputKind::is_chat_turn` carries the rule and `Turn::is_chat_turn`
delegates to it, so a display surface holding only the projected kind asks the
same question as `/stop`. That matters because the trace viewer **numbers only
chat turns**: a session with two messages and one compaction has three `turns`
rows but rendered "Turn #3", disagreeing with the transcript the user had just
read. The mirror of the rule lives in `app/web/src/types/trace.ts` (`isChatTurn`)
and `crates/trace/tests/web_trace_types_sync.rs` pins the two together — it
fails if the TS union gains a kind Rust lacks, or if the non-chat set stops
matching.

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

`Cancelled` carries a `reason: CancelReason` (`UserPreempt`, `SystemCrash`, `SubagentTimeout`, `ParentCancelled`, `ParentDeleted`, `OperatorCancel`, `UserStopped`) and `partial_artifacts: Vec<SpanId>` — the spans that completed (or partially completed) before the cancel. Both fields are nested **inside** the `TurnStatus::Cancelled { reason, partial_artifacts }` variant; the top-level `Turn` exposes `emitted_span_ids` for general progress indexing. The field is reserved for a future prompt-assembly preamble that would surface those spans to the next turn's LLM; no consumer reads it today, and every production cancel path currently passes an empty list. Content lives only in the trace; the field is indices.

`SystemCrash` is used when Baybo owns the cleanup after execution disappeared:
the boot recovery sweep rolls turns left non-terminal by a prior process death to
`Cancelled { SystemCrash }`, and the in-process actor panic runner does the same
for the panicked session's active chat turns.

### Two orthogonal descriptors: input kind and origin

A turn is described along two independent axes, each with one source of truth — replacing a single overloaded `kind` that conflated "what payload" with "which trigger":

```rust
// input kind — what payload fed the turn; a projection of TurnInput.
// Display / the denormalised `turns.kind` column only.
pub enum TurnInputKind { UserChat, Cron, CronNotification, Compact, Spawned, SubagentNotification }
```

- **input kind** (`TurnInputKind`) — `Turn::input_kind()`, projected from `TurnInput`. `TurnInput` is a strongly typed payload enum whose variants line up 1:1 with `TurnInputKind`. `UserChat`, `Cron`, `Spawned`, and `SubagentNotification` are chat turns; `Compact` (a foreground maintenance command, no reply) and `CronNotification` (the delivery of a one-shot cron fire's result into the conversation that scheduled it) are not — see the subset table above. `CronNotification` still opens a real turn: its `Completed { reply_ordinal }` edge is what drives the push dispatcher off the row it just appended, exactly as a user turn does.
- **origin** (`baybo_model::TriggerKind`, stored on `Turn.origin`) — the owning session's root trigger, recorded **as-is** at creation. It is *not* asserted against the payload: `/compact` can run inside a `User`-trigger session while carrying a `Compact` input. Subagent turns record `origin = Spawned` (their session's inherited root).

`TurnOutput` does not split this way — it has only `Message` and `Structured`, the two shapes any turn can produce.

### Turn owns its behavior

`Turn` is not a passive data struct — it encapsulates the state machine:

- `Turn::new(session_id, origin, input, parent_turn_id)` — constructor with ULID, `Pending` status, timestamps. `origin` is supplied by the caller; `input_kind` is projected from `input`.
- `Turn::transition(target, ...)` — validates transition, mutates status/timestamps, returns a `TurnTransition` legality receipt (consumed and dropped by `TurnLifecycle`; the per-transition audit table was retired in the 2026-07 unused-column audit). `transition_at(target, ..., at)` / `cancel_at(reason, artifacts, at)` are explicit-timestamp variants reserved for the boot-recovery sweep, which must backdate `ended_at` to the last observed activity rather than the boot wall-clock; live callers use `transition` / `cancel`.
- Convenience methods: `start()`, `complete(output)`, `fail(reason)`, `cancel(reason, partial_artifacts)`, `stuck(reason)`, `recover(reason)`
- `Turn::is_terminal()` — true for `Completed | Cancelled | Failed`
- `Turn::is_chat_turn()` — the chat-turn subset predicate (see above)
- `TurnStatus::needs_recovery()` — true for `Pending | InProgress | Stuck` (consumed by admin queries that surface in-flight turns)

Timestamp rules are enforced inside `transition()`:
- `started_at` is set on first entry to `InProgress` (not on recovery re-entry)
- `ended_at` is set on entry to any terminal state (`Completed`, `Cancelled`, `Failed`)

This keeps the state machine invariants co-located with the type and makes them testable without any storage dependency.

### TurnLifecycle is a thin persistence orchestrator

`TurnLifecycle` does only: load from store → call `turn.transition()` → `store.save()`. No state machine logic in the orchestrator. It additionally owns:

- A `tokio::sync::broadcast` bus that publishes a `TurnLifecycleEvent` (id, session, parent, phase, input kind; the `Completed` phase additionally carries the reply's persisted `session_messages.ordinal`) on `Pending → InProgress` and on every `Completed | Failed | Cancelled` transition. Subscribers: the subagent runtime waits for terminal phases, the TurnState projection treats every phase as a recompute trigger, and the push dispatcher filters `Completed` events to the kinds a user is meant to read (`UserChat`, `Cron` — confirmed against the session's `conversation` marker — and `CronNotification`). Lagging subscribers must reconcile via store reads such as `list_by_session` / `active_turn_started_at` — a dropped event is not re-published.
- A `TurnCancellationRegistry` mapping `TurnId → CancellationToken` for in-flight turns. `TurnLifecycle::cancel` trips the registered token *before* flipping the row, so the running execution observes the cancel before terminal-state observers do. `register_running` returns a RAII `TurnCancellationGuard` that unregisters on drop, so an early `?` from the agent loop can't leak entries.

### Recovery

The state-machine and storage shape support recovery of non-terminal turns
(`Pending` / `InProgress` / `Stuck`). `baybo_agent::recovery` owns the cross-table
repair because it has both turn and trace stores:

- Boot recovery scans all non-terminal turns from the prior process, closes any
  half-open trace rows at the last observed activity time, and calls
  `TurnLifecycle::cancel_at(..., SystemCrash, ...)`.
- Actor panic recovery scans only the panicked session's active chat turns,
  closes their half-open trace rows at the actor crash time, and cancels them as
  `SystemCrash`.

`TurnStatus::Cancelled.partial_artifacts` remains the resume hook for spans that
completed before cancellation; content itself lives in trace.

### Turn hierarchy

Turns support parent-child relationships via `parent_turn_id`. Child success/failure does not auto-rewrite parent state — that's upper-layer business logic. `list_children()` returns only direct children.

### Per-trigger queue / preempt policy

The turn state machine itself is trigger-agnostic, but the actor that drives it follows per-trigger policy. The actor is a serial loop over a priority mailbox, so nothing a *trigger* carries preempts a running turn — only the subagent token tree does:

| Session trigger | New trigger arriving while a turn is `InProgress`                |
| --------------- | ---------------------------------------------------------------- |
| `User`          | Queue / inject: lands in the actor mailbox at `Trigger` priority (see below) |
| `Cron`          | Queue: actor mailbox holds it until the current turn is terminal  |
| Subagent (any)  | Preempt: parent's cancellation token tree propagates downward     |

A running turn drains the leading run of non-slash user inputs at each tool boundary and injects them mid-turn (non-preemptive — never mid-LLM-call); anything still queued when the turn ends is coalesced into the next turn. Preemption is not implemented — `CancelReason::UserPreempt` has no production producer today; the out-of-band `/stop` is the only way to cancel a running turn.

`/stop` cancels the in-flight turn (and every in-flight descendant subagent) with `Cancelled { UserStopped, ... }` — suppression of the terminal `BackgroundJobFinished` delivery comes from `/stop` draining the supervisor's in-flight background-subagent registry (each child's wait task finds its entry gone and drops the delivery), so a stopped result never repopulates the parent notification buffer; `UserStopped` is the audit reason stamped on the cancelled rows.

### Collaboration with Trace

| Dimension      | Turn                                                 | Trace                                                       |
| -------------- | ---------------------------------------------------- | ----------------------------------------------------------- |
| Focus          | State                                                | Content                                                     |
| Key fields     | `status`, timestamps, hierarchy, `final_result`      | `step_id`, `span_id`, kind-specific input/output/provenance |
| Sensitive data | Sanitized JSON only                                  | Sanitized payloads/summaries only                           |

`TurnLifecycle` lives in this crate; `SpanRecorder` (in `baybo-trace`) is its peer facade. They do not share a transaction; cross-table consistency is reconciled by the recovery scan (per-table transactions, eventually consistent).

## Constraints

- `input` / `final_result` / `TurnStatus::Cancelled.partial_artifacts` store sanitized JSON / span-id lists only — sensitive values must already be placeholders
- `Turn.origin` is supplied by the caller at `TurnLifecycle::start_turn` (via `TurnSpec.origin`) and passed straight into `Turn::new`; it is not validated against the payload. Only `input_kind` is projected from `input`
- Does not depend on `trace`, `llm`, `tools`, or `agent`. Depends only on `baybo-model` (IDs) and `baybo-store` (the `TurnStore` trait + its row DTO).
- The in-memory `TurnStore` fake in `test_support` is gated behind the `test-support` feature so it never ships in release builds. Downstream test crates pull it in via `baybo-turn = { workspace = true, features = ["test-support"] }`.

## Collaboration

| Module    | Role                                                                                                      |
| --------- | --------------------------------------------------------------------------------------------------------- |
| `agent`   | Consumes `TurnLifecycle` to drive turns through the agent loop; supplies the cancellation tokens that `register_running` tracks |
| `trace`   | Provides `SpanId`; `TurnStatus::Cancelled.partial_artifacts` references trace spans; recovery coordinates with the trace scan    |
| `store`   | Owns the `TurnStore` trait + its `TurnRow` DTO and `StorageError`; this crate converts `Turn` ↔ rows |
| `storage` | Provides the sqlite implementation of `TurnStore` (from `baybo-store`) over the `turns` table, shuttling rows; depends on `baybo-turn` only as a dev-dependency |
| `session` | `Session.trigger.kind()` is recorded as `Turn.origin`; `Lineage` consumes `parent_turn_id`                       |
