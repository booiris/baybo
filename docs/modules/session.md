# session - Session Lifecycle Manager

## Overview

The `session` crate owns `SessionError` and the `SessionManager` that implements session lifecycle logic. Domain types (`Session`, `SessionState`, `User`, `ChannelType`, `SessionTrigger`, `SessionParentLink`, `ParentLinkKind`, `SystemTrigger`) live in `aura-model`; the `SessionStore` persistence trait lives in `aura-storage`. `aura-session` depends on both and wraps the store behind the manager API.

**Design principle**: types and store interface are pushed down into lower crates (`aura-model`, `aura-storage`) so this crate can consume the store without creating a dependency cycle. `SessionManager` itself is the only business-logic resident. Concrete storage implementations live in `storage`; higher-level orchestration (Router, Actor) lives in `agent`, which re-exports `SessionManager` for convenience.

## Design Decisions

### Session isolation policy

The same `User` has independent sessions on different channels. Sessions are keyed purely by `session_id` (string); `user` and `channel` are persisted on new sessions but are not part of the lookup key. Callers are responsible for constructing a `session_id` that encodes the desired isolation — e.g. the CLI uses `cli-<uuid>` (one id per process). Cron sessions are no longer deterministic: each cron fire mints a fresh UUID-id session via `create_session_with_trigger(SessionTrigger::Cron { ... })`.

**Invariant**: when `get_or_create` needs to insert a new session (either the id was missing or the prior session expired), it must persist it under the caller-supplied `session_id`. Callers rely on the id staying stable so that subsequent `touch` / `route` calls resolve the same session.

### Trigger and parent link

Every `Session` carries an explicit `trigger: SessionTrigger` (defaulting to `User`) recording why it was created:

- `User` — `/new`, channel first message, etc.
- `Cron { cron_job_id, scheduled_fire_time }` — minted on each cron fire.
- `System(SystemTrigger)` — Aura-internal initiative (`PeriodicReview`, `ContextCompaction`, `MemoryConsolidation`, `SkillDiscovery`); the inner enum's tag is `system_kind` to avoid colliding with `SessionTrigger`'s outer `kind` tag when flattened.
- `Parent { link_kind }` — created by another session (sub-agent or fork). The actual parent reference lives in `Session::parent_link`.

`SessionParentLink { session_id, kind: ParentLinkKind, at_job_id, at_span_id }` records the cross-session boundary. `ParentLinkKind` is one of `SubAgent` (parent span synchronously waits for the child), `Fork` (independent branch with no result feedback), `CronChain`, or `SystemContinuation`. `at_span_id` is only set for `SubAgent`.

### Cross-session fork

`SessionManager::fork_session(parent_session_id, at_job_id) -> Session` creates a new session that:

1. Inherits the parent's `user` and `channel`.
2. Sets `trigger = SessionTrigger::Parent { link_kind: Fork }` and `parent_link.kind = Fork`.
3. Copies the parent's `messages` so the child's agent loop sees the conversation history. Edits on the child don't mutate the parent on disk.

The fork API is also exposed at `POST /v1/sessions/{id}/fork` on the gateway admin surface; the gateway validates that `at_job_id` actually belongs to the parent before delegating. Message-level truncation at the exact `at_job_id` cutoff (rather than full history copy) requires job-tagged messages and is a deferred follow-up.

### Actor model serialization

Aura uses one Actor per session. All messages targeting the same session (user input, cron, rollback, timeout) are queued through the actor handle and consumed sequentially. Therefore, `SessionStore` implementations do not need to handle write conflicts on the same `session_id` — that guarantee comes from the Actor model.

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

- Session IDs are caller-supplied opaque strings; typical producers prefix a UUID v4 to namespace by channel (e.g. `cli-<uuid>`). Cron and forks always mint fresh UUID-id sessions. `SessionManager::create_session` generates a bare UUID v4 when no id is requested.
- `Session`, `User`, `ChannelType`, `SessionState` live in `aura-model`; `SessionStore` lives in `aura-storage`.
- `SessionManager` owns lifecycle logic; `StorageError` is wrapped into `SessionError::Storage` at the manager boundary; `agent` re-exports the manager for convenience.

## Collaboration

| Module    | Role                                                                                    |
| --------- | --------------------------------------------------------------------------------------- |
| `model`   | Owns `Session`, `User`, `ChannelType`, `SessionState` — pure data types                 |
| `storage` | Defines `SessionStore` trait and provides `LibsqlSessionStore`                          |
| `agent`   | Re-exports `SessionManager`; Router calls it; `AgentActor` holds the `Session` instance |
| `hook`    | `SessionCreated` / `SessionDestroyed` hook points for welcome messages, audit, cleanup  |
