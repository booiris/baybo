# session - Session Types, Store Trait, and Manager

## Overview

The `session` crate owns session-related domain types (`Session`, `SessionState`, `User`, `ChannelType`), the `SessionStore` persistence trait, and the `SessionManager` that implements session lifecycle logic.

**Design principle**: session is a self-contained module — types, store interface, and business logic live together. Concrete storage implementations (e.g. libsql) live in `storage`; higher-level orchestration (Router, Actor) lives in `agent`.

## Design Decisions

### Session isolation policy

The same `User` has independent sessions on different channels. Sessions are keyed purely by `session_id` (string); `user` and `channel` are persisted on new sessions but are not part of the lookup key. Callers are responsible for constructing a `session_id` that encodes the desired isolation — e.g. CLI uses `cli-<uuid>` (one id per process), cron uses `cron-<user>-<channel>` (stable across triggers).

**Invariant**: when `get_or_create` needs to insert a new session (either the id was missing or the prior session expired), it must persist it under the caller-supplied `session_id`. Callers rely on the id staying stable so that subsequent `touch` / `route` calls resolve the same session.

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
- `extra`: reserved extension fields for experimental features or plugin state.

## Constraints

- Session IDs are caller-supplied opaque strings; typical producers prefix a UUID v4 to namespace by channel (e.g. `cli-<uuid>`, `cron-<user>-<channel>`). `SessionManager::create_session` generates a bare UUID v4 only when no id is requested.
- `SessionStore` trait lives in this crate; concrete implementations live in `storage`
- `SessionManager` owns lifecycle logic; `agent` re-exports it for convenience

## Collaboration

| Module    | Role                                                                                    |
| --------- | --------------------------------------------------------------------------------------- |
| `agent`   | Re-exports `SessionManager`; Router calls it; `AgentActor` holds the `Session` instance |
| `storage` | Provides concrete `SessionStore` implementations (e.g. `LibsqlSessionStore`)            |
| `hook`    | `SessionCreated` / `SessionDestroyed` hook points for welcome messages, audit, cleanup  |
