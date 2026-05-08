# session - Session Lifecycle Manager

## Overview

The `session` crate owns `SessionError` and the `SessionManager` that implements session lifecycle logic. Domain types (`Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `SystemReason`, `Lineage`, `LineageKind`) live in `aura-model`; the `SessionStore` persistence trait lives in `aura-storage`. `aura-session` depends on both and wraps the store behind the manager API.

A `Session` is the top of one trace tree. There is exactly one trace per session — fork and subagent spawn create new sessions with `Lineage` pointers, never new trees rooted in the same session.

The conversation transcript itself is **not** carried on `Session`. It lives in `aura_context::ContextManager` while the actor is alive; the agent loop persists each appended message and each `/compact` apply through `SessionManager::append_session_message` / `apply_session_compaction`, and the router seeds it from `load_active_session_messages` on cold start.

**Design principle**: types and store interface are pushed down into lower crates (`aura-model`, `aura-storage`) so this crate can consume the store without creating a dependency cycle. `SessionManager` itself is the only business-logic resident. Concrete storage implementations live in `storage`; higher-level orchestration (Router, Actor) lives in `agent`, which re-exports `SessionManager` for convenience.

## Manager surface

`SessionManager` wraps `Arc<dyn SessionStore>` and exposes:

- Lifecycle: `create_session` / `create_session_with_trigger` / `create_spawned_session` / `get_or_create` / `get` / `list` / `delete` / `cleanup_expired` / `touch`.
- Transcript brokerage (thin pass-throughs to `SessionStore`): `append_session_message`, `apply_session_compaction`, `load_active_session_messages`, `latest_session_ordinal`, `load_session_messages_with_supersede`, `history`. The agent loop calls these every turn so the in-memory `ContextManager` stays in sync with `session_messages` rows. The leading system message rides the same log — no separate dedup table.

## Design Decisions

### Session isolation policy

The same `User` has independent sessions on different channels. Sessions are keyed purely by `session_id` (string); `user` and `channel` are persisted on new sessions but are not part of the lookup key. Callers are responsible for constructing a `session_id` that encodes the desired isolation — e.g. CLI uses `cli-<uuid>` (one id per process), cron uses `cron-<user>-<channel>` (stable across triggers).

**Invariant**: when `get_or_create` needs to insert a new session (either the id was missing or the prior session expired), it must persist it under the caller-supplied `session_id`. Callers rely on the id staying stable so that subsequent `touch` / `route` calls resolve the same session.

### Trigger and lineage are orthogonal

`Session.trigger: TriggerSource` records the **business source** that started this session (`User`, `Cron { cron_job_id }`, `System { reason: SystemReason }`). `Session.lineage: Option<Lineage>` records the **parent relationship** when the session was spawned from another (`Subagent` or `UserFork { fork_at_job_id, prefix_state_hash }`).

A spawned session's `trigger` is **inherited from the root** session (so a subagent spawned by a cron-triggered session has `trigger = Cron { ... }`), making "is this work cron-driven?" an O(1) field read. `Lineage` separately records the direct parent. `SystemReason` is a closed strong-typed enum extended by adding variants — never by string.

### Root-session ancestry

`Session.root_session_id` is self-referential when the session has no `Lineage` parent, and otherwise transitively points to the topmost ancestor. This lets ancestry queries (cost roll-ups, audit aggregation) hit one row instead of recursing.

### Actor-model write serialization

One `AgentActor` per session. All messages targeting the same session (user input, cron, rollback, timeout) are queued through the actor handle and consumed sequentially. Therefore `SessionStore` implementations do not need to defend against concurrent writes on the same `session_id` — that guarantee comes from the actor model and is what lets `append_session_message` use a single `INSERT … SELECT MAX(ordinal)+1` without locking. Re-introducing concurrent paths into the session would invalidate the storage contract.

### Source deletion is rejected when live forks exist

`SessionStore::delete(source_id)` returns `Err(SessionError::HasLiveForks { fork_session_ids })` when any session has a `Lineage::UserFork` pointing into the source. Callers must delete the forks first or accept the error. There is no materialize-on-delete escape hatch — the case is rare enough to surface as an error rather than silently rewrite snapshots. The same delete cascades the session's `session_messages` rows so a stranded transcript can never outlive its parent.

### Subagent parent deletion drains the subtree first

When a session with an in-flight subagent is deleted, the subagent's cancellation token is tripped (`Cancelled { ParentDeleted }`) **before** the parent row is removed. This drains the entire descendant subtree before the delete completes, so a parent never disappears while a child is still running tools or holding LLM state.

### Session ID conventions

`SessionId` is a caller-supplied opaque string. Producers prefix a UUID v4 to namespace by channel:
- CLI: `cli-<uuid>` — one id per process.
- Cron: `cron-<user>-<channel>` — stable across triggers so repeated firings resume the same session.
- Subagent / fork: minted by `create_spawned_session` from the parent's id and the lineage kind.

`SessionManager::create_session` generates a bare UUID v4 only when no id is requested. A spawned session's `trigger` must equal its root session's `trigger` — enforced at `create_session` time.

### `SessionState` fields

- `active_skills`: skill names invoked this turn (slash-command or `/mention`, score ≥ 0.9). Repopulated each turn by `AgentLoop` from the active band; pure provenance for trace/CLI display. Tool governance is computed separately as the union of those skills' `allowed_tools` lists. See [`agent.md`](agent.md).
- `compression_count`: incremented after each successful context compression. Used by monitoring / strategy switching to detect runaway growth.
- `approved_resources`: tool resources the user has granted permanent approval for in this session, populated on each `ApproveAlways` decision. See [`tools.md`](tools.md).
- `extra`: reserved extension fields for experimental features or plugin state.

### Soul-version drift

`Session.bound_soul_version` is locked when the session is created. Each `Job` records its own `effective_soul_version` at start time. If they diverge (hot reload changed the soul mid-session), the job records a `DriftRecord` in `provenance_drift`. The session's contract stays anchored even when the runtime config moves underneath it.

### Session timeout flow

1. Periodic cleanup task triggers `SessionManager::cleanup_expired()`
2. Compute cutoff time: `now - session_timeout`
3. `list_expired(cutoff)` returns expired session IDs
4. For each: `SessionStore::delete(session_id)` removes the session row and cascades the `session_messages` log.

## Collaboration

| Module    | Role                                                                                                |
| --------- | --------------------------------------------------------------------------------------------------- |
| `model`   | Defines `Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `Lineage`               |
| `storage` | Defines `SessionStore` trait + libsql impl; defines `StoredMessage` for transcript replay          |
| `context` | Owns the in-memory transcript via `ContextManager`; agent loop brokers persistence via this crate  |
| `agent`   | Re-exports `SessionManager`; Router calls it; `AgentActor` holds the `Session` instance            |
| `cli` / `gateway` | Operator-facing surfaces consume `aura_agent::SessionManager`                              |
