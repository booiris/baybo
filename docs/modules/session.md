# session - Session Lifecycle Manager

## Overview

The `session` crate owns `SessionError` and the `SessionManager` that implements session lifecycle logic. Domain types (`Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `SystemReason`, `Lineage`, `LineageKind`) live in `aura-model`; the `SessionStore` persistence trait lives in `aura-storage`. `aura-session` depends on both and wraps the store behind the manager API.

A `Session` is the top of one trace tree. There is exactly one trace per session — fork and subagent spawn create new sessions with `Lineage` pointers, never new trees rooted in the same session.

**Design principle**: types and store interface are pushed down into lower crates (`aura-model`, `aura-storage`) so this crate can consume the store without creating a dependency cycle. `SessionManager` itself is the only business-logic resident. Concrete storage implementations live in `storage`; higher-level orchestration (Router, Actor) lives in `agent`, which re-exports `SessionManager` for convenience.

## Design Decisions

### Session isolation policy

The same `User` has independent sessions on different channels. Sessions are keyed purely by `session_id` (string); `user` and `channel` are persisted on new sessions but are not part of the lookup key. Callers are responsible for constructing a `session_id` that encodes the desired isolation — e.g. CLI uses `cli-<uuid>` (one id per process), cron uses `cron-<user>-<channel>` (stable across triggers).

**Invariant**: when `get_or_create` needs to insert a new session (either the id was missing or the prior session expired), it must persist it under the caller-supplied `session_id`. Callers rely on the id staying stable so that subsequent `touch` / `route` calls resolve the same session.

### Trigger and lineage are orthogonal

`Session.trigger: TriggerSource` records the **business source** that started this session (`User`, `Cron { cron_job_id }`, `System { reason: SystemReason }`). `Session.lineage: Option<Lineage>` records the **parent relationship** when the session was spawned from another (`Subagent` or `UserFork { fork_at_job_id, prefix_state_hash }`).

A spawned session's `trigger` is **inherited from the root** session (so a subagent spawned by a cron-triggered session has `trigger = Cron { ... }`), making "is this work cron-driven?" an O(1) field read. `Lineage` separately records the direct parent. `SystemReason` is a closed strong-typed enum extended by adding variants — never by string.

### Root-session ancestry

`Session.root_session_id` is self-referential when the session has no `Lineage` parent, and otherwise transitively points to the topmost ancestor. This lets ancestry queries (cost roll-ups, audit aggregation) hit one row instead of recursing.

### Actor model serialization

Aura uses one Actor per session. All messages targeting the same session (user input, cron, rollback, timeout) are queued through the actor handle and consumed sequentially. Therefore, `SessionStore` implementations do not need to handle write conflicts on the same `session_id` — that guarantee comes from the Actor model.

`UserChat` triggers are **preempted** when a new user message arrives mid-job (current job ends in `Cancelled { UserPreempt, partial_artifacts }`). `Cron` and `System` triggers are **queued** in the actor mailbox. Cancellation propagation is via a `tokio_util::sync::CancellationToken` tree owned by the actor; mailbox `AgentMessage::Cancel { reason }` records the audit event.

### Fork is recorded in lineage, not in a separate table

`UserFork { fork_at_job_id, prefix_state_hash }` is the entire on-disk record of a fork. The new session's job prefix is **not copied** — it is read from the source session via a view-layer UNION (`list_jobs(new_session)` = source jobs up to `fork_at_job_id` ∪ new session's own jobs). API responses tag inherited rows with `is_inherited: true` so UIs can render lineage without rewriting IDs.

### Source deletion is rejected when live forks exist

`SessionStore::soft_delete(source_id)` returns `Err(SessionError::HasLiveForks { fork_session_ids })` when any non-deleted session has a `Lineage::UserFork` pointing into the source. Callers must delete the forks first or accept the error. There is no materialize-on-delete escape hatch — the case is rare enough to surface as an error rather than silently rewrite snapshots.

### Subagent parent deletion cascades cancel

When a session with an in-flight subagent is deleted, the subagent's cancellation token is tripped first (`Cancelled { ParentDeleted }`), then the session is marked deleted. This drains the entire descendant subtree before the soft-delete completes.

### Soul-version drift

`Session.bound_soul_version` is locked when the session is created. Each `Job` records its own `effective_soul_version` at start time. If they diverge (hot reload changed the soul mid-session), the job records a `DriftRecord` in `provenance_drift`. The session's contract stays anchored even when the runtime config moves underneath it.

### Session timeout flow

1. Periodic cleanup task triggers `SessionManager::cleanup_expired()`
2. Compute cutoff time: `now - session_timeout`
3. `list_expired(cutoff)` returns expired session IDs
4. For each: send `SessionTimeout` to the corresponding `AgentActor` → cleanup → `delete(session_id)`

### SessionState use cases

- `active_skills`: names of skills explicitly invoked this turn (slash-command or inline `/mention`, score ≥ 0.9). Repopulated every turn by `AgentLoop` from the active band — kept as a `Vec<String>` because multiple skills can be active simultaneously. Pure provenance for trace/CLI display; tool governance is computed separately as the union of those skills' `allowed_tools` lists.
- `compression_count`: incremented after each context compression, useful for monitoring or strategy switching.
- `approved_resources`: tool resources the user has granted permanent approval for in this session. Appended on each `ApproveAlways` decision by the approval gate and persisted with the session so restored sessions remember the grants. Matching semantics live in `aura_model::approval`.
- `extra`: reserved extension fields for experimental features or plugin state.

## Constraints

- Session IDs are caller-supplied opaque strings; typical producers prefix a UUID v4 to namespace by channel (e.g. `cli-<uuid>`, `cron-<user>-<channel>`). `SessionManager::create_session` generates a bare UUID v4 only when no id is requested.
- `Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `SystemReason`, `Lineage`, `LineageKind` live in `aura-model`; `SessionStore` lives in `aura-storage`.
- `SessionManager` owns lifecycle logic; `StorageError` is wrapped into `SessionError::Storage` at the manager boundary; `agent` re-exports the manager for convenience.
- A spawned session's `trigger` must equal its root session's `trigger`. Enforced at `create_session` time.
- `soft_delete` must reject when live forks reference the session, and must drain in-flight subagents before completing.

## Collaboration

| Module    | Role                                                                                               |
| --------- | -------------------------------------------------------------------------------------------------- |
| `model`   | Owns `Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `Lineage`, `SystemReason`  |
| `storage` | Defines `SessionStore` trait and provides `LibsqlSessionStore`                                     |
| `agent`   | Re-exports `SessionManager`; Router calls it; `AgentActor` holds the `Session` instance            |
