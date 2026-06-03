# session - Session Lifecycle Manager

## Overview

The `session` crate owns the session lifecycle vertical: `SessionError` and the `SessionManager` business-logic facade. The `SessionStore` / `SessionSummaryStore` traits and their per-row `StoredMessage` / `SessionSummaryRow` value types live in the `aura-store` ports crate; domain types (`Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `Lineage`, `LineageKind`) live in `aura-model`. The libsql implementations of both store traits live in `aura-storage`, which implements the `aura-store` contracts; `aura-session` calls them.

A `Session` is the top of one trace tree. There is exactly one trace per session — subagent and maintenance spawn create new sessions with `Lineage` pointers, never new trees rooted in the same session.

The conversation transcript itself is **not** carried on `Session`. It lives in `aura_context::ContextManager` while the actor is alive; the agent loop persists each appended message and each `/compact` apply through `SessionManager::append_session_message` / `apply_session_compaction`, and the router seeds it from `load_active_session_messages` on cold start.

**Design principle**: each domain crate owns its business-logic vertical (manager + error + test-support fake) while the `*Store` trait contract lives in the `aura-store` ports crate. Downstream callers depend on the manager crate for logic and on `aura-store` for the trait. `aura-storage` keeps only the libsql implementations and exposes a `Store` bundle for assembly-layer wiring. Higher-level orchestration (Router, Actor) lives in `agent`, which re-exports `SessionManager` for convenience.

## Manager surface

`SessionManager` wraps two stores — `store: Arc<dyn SessionStore>` (the session rows + transcript) and `summary_store: Arc<dyn SessionSummaryStore>` (per-session summary-cursor metadata) — both required at `new()`. It exposes:

- Lifecycle: `create_session` / `create_session_with_trigger` / `create_spawned_session` / `get_or_create` / `get` / `list` / `delete` / `idle_sessions` / `touch`. `idle_sessions(threshold)` only **returns** the IDs of sessions idle past the threshold — it never deletes a row (its sole consumer is the actor reaper, which evicts in-memory actors, not rows).
- Transcript brokerage (thin pass-throughs to `SessionStore`): `append_session_message`, `apply_session_compaction`, `load_active_session_messages`, `latest_session_ordinal`, `load_session_messages_with_supersede`, `history`. The agent loop calls these every turn so the in-memory `ContextManager` stays in sync with `session_messages` rows. The leading system message rides the same log — no separate dedup table.

## Design Decisions

### Session isolation policy

The same `User` has independent sessions on different channels. Sessions are keyed purely by `session_id` (string); `user` and `channel` are persisted on new sessions but are not part of the lookup key. Callers are responsible for constructing a `session_id` that encodes the desired isolation — e.g. CLI uses `cli-<uuid>` (one id per process), cron uses `cron-<user>-<channel>` (stable across triggers).

**Invariant**: when `get_or_create` needs to insert a new session (either the id was missing or the prior session expired), it must persist it under the caller-supplied `session_id`. Callers rely on the id staying stable so that subsequent `touch` / `route` calls resolve the same session.

### Trigger and lineage are orthogonal

`Session.trigger: TriggerSource` records the **business source** that started this session (`User`, `Cron { cron_job_id }`). `Session.lineage: Option<Lineage>` records the **parent relationship** when the session was spawned from another (`Subagent`).

A spawned session's `trigger` is **inherited from the root** session (so a subagent spawned by a cron-triggered session has `trigger = Cron { ... }`), making "is this work cron-driven?" an O(1) field read. `Lineage` separately records the direct parent.

### Root-session ancestry

`Session.root_session_id` is self-referential when the session has no `Lineage` parent, and otherwise transitively points to the topmost ancestor. This lets ancestry queries (cost roll-ups, audit aggregation) hit one row instead of recursing.

### Actor-model write serialization

One `AgentActor` per session. All messages targeting the same session (user input, cron, rollback, timeout) are queued through the actor handle and consumed sequentially. Therefore `SessionStore` implementations do not need to defend against concurrent writes on the same `session_id` — that guarantee comes from the actor model and is what lets `append_session_message` use a single `INSERT … SELECT MAX(ordinal)+1` without locking. Re-introducing concurrent paths into the session would invalidate the storage contract.

### Source deletion cascades the transcript

`SessionStore::delete(source_id)` runs the `session_messages` cascade and the session-row delete inside one `BEGIN IMMEDIATE` write transaction so a stranded transcript can never outlive its parent.

### Subagent parent deletion drains the subtree first

When a session with an in-flight subagent is deleted, the subagent's cancellation token is tripped (`Cancelled { ParentDeleted }`) **before** the parent row is removed. This drains the entire descendant subtree before the delete completes, so a parent never disappears while a child is still running tools or holding LLM state.

### Session ID conventions

`SessionId` is a caller-supplied opaque string. Producers prefix a UUID v4 to namespace by channel:
- CLI: `cli-<uuid>` — one id per process.
- Cron: `cron-<user>-<channel>` — stable across triggers so repeated firings resume the same session.
- Subagent / maintenance: minted by `create_spawned_session` from the parent's id and the lineage kind.

`SessionManager::create_session` generates a bare UUID v4 only when no id is requested. A spawned session's `trigger` must equal its root session's `trigger` — enforced at `create_session` time.

### `SessionState` fields

- `compression_count`: incremented after each successful context compression. Used by monitoring / strategy switching to detect runaway growth.
- `approved_resources`: tool resources the user has granted permanent approval for in this session, populated on each `ApproveAlways` decision. See [`tools.md`](tools.md).
- `last_llm`: per-session LLM pin — the `aura.json` entry name this session's turns resolve against, or `None` to follow `default-llm`. Read by the actor spawner as the loop's `initial_llm`; a live actor is re-pinned via `AgentMessage::SetModel`. Set from the chat UI via `PUT /v1/chat/sessions/:id/model`. See [`agent.md`](agent.md) "Per-session model selection".
- `extra`: reserved extension fields for experimental features or plugin state.

### Soul-version drift

`Session.bound_soul_version` is locked when the session is created. Each `Job` records its own `effective_soul_version` at start time. If they diverge (hot reload changed the soul mid-session), the job records a `DriftRecord` in `provenance_drift`. The session's contract stays anchored even when the runtime config moves underneath it.

### Idle actor reaping — never row deletion

Session rows and their transcripts are core user data and are **never** dropped by any background sweep (see CLAUDE.md, "Session data is core data — never delete"). Idleness only evicts the in-memory `AgentActor`, never the durable row:

1. The actor reaper (`AgentSupervisor::reap_idle`) periodically calls `SessionManager::idle_sessions(threshold)`.
2. That computes `cutoff = now - threshold` and returns `SessionStore::list_expired(cutoff)` — a list of candidate session IDs and nothing more. The method's doc-comment states it "must never become a delete."
3. For each candidate that has a live registered actor (and no in-flight background subagents), the supervisor sends `AgentMessage::ActorStop`, which trips the actor's cancellation token and lets the registry guard drop the in-memory entry.
4. The session row, transcript, summary cursor, and channel binding all stay live; the next user message re-hydrates a fresh actor from the store. Dropping the actor is therefore lossless.

## Collaboration

| Module    | Role                                                                                                |
| --------- | --------------------------------------------------------------------------------------------------- |
| `model`   | Defines `Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `Lineage`               |
| `storage` | Implements `SessionStore` / `SessionSummaryStore` against libsql; pulls the traits from `aura-store` (no `aura-session` dependency) |
| `context` | Owns the in-memory transcript via `ContextManager`; agent loop brokers persistence via this crate  |
| `agent`   | Re-exports `SessionManager`; Router calls it; `AgentActor` holds the `Session` instance            |
| `cli` / `gateway` | Operator-facing surfaces consume `aura_agent::SessionManager`                              |
