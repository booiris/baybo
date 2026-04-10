# session - Session Types, Store Trait, and Manager

## Overview

The `session` crate owns session-related domain types (`Session`, `SessionState`, `User`, `ChannelType`), the `SessionStore` persistence trait, and the `SessionManager` that implements session lifecycle logic.

**Design principle**: session is a self-contained module — types, store interface, and business logic live together. Concrete storage implementations (e.g. libsql) live in `storage`; higher-level orchestration (Router, Actor) lives in `agent`.

## Design Decisions

### Session isolation policy

The same `User` has independent sessions on different channels. Each `(user_id, channel)` pair maps to an independent session. `get_or_create` must consider both user and channel.

### Actor model serialization

Aura uses one Actor per session. All messages targeting the same session (user input, cron, rollback, timeout) are queued through the actor handle and consumed sequentially. Therefore, `SessionStore` implementations do not need to handle write conflicts on the same `session_id` — that guarantee comes from the Actor model.

### Session timeout flow

1. Periodic cleanup task triggers `SessionManager::cleanup_expired()`
2. Compute cutoff time: `now - session_timeout`
3. `list_expired(cutoff)` returns expired session IDs
4. For each: send `SessionTimeout` to the corresponding `AgentActor` → cleanup → `delete(session_id)`

### SessionState use cases

- `active_skill`: set during multi-turn skill flows, cleared on completion. `AgentLoop` uses it to route to skill handlers.
- `compression_count`: incremented after each context compression, useful for monitoring or strategy switching.
- `extra`: reserved extension fields for experimental features or plugin state.

## Constraints

- Session IDs use UUID v4 (random, no ordering needed)
- `SessionStore` trait lives in this crate; concrete implementations live in `storage`
- `SessionManager` owns lifecycle logic; `agent` re-exports it for convenience

## Collaboration

| Module    | Role                                                                                    |
| --------- | --------------------------------------------------------------------------------------- |
| `agent`   | Re-exports `SessionManager`; Router calls it; `AgentActor` holds the `Session` instance |
| `storage` | Provides concrete `SessionStore` implementations (e.g. `LibsqlSessionStore`)            |
| `hook`    | `SessionCreated` / `SessionDestroyed` hook points for welcome messages, audit, cleanup  |
