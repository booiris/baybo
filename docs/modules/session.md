# session - Session Types and Identity

## Overview

The `session` crate defines session-related domain types (`Session`, `SessionState`, `User`, `ChannelType`) and the `SessionError` error type.

**Design principle**: domain types live in their own crate; Store traits live in `storage`; business logic lives in `agent`. `SessionStore` is defined in `storage::session`; `SessionManager` lives in `agent::session`.

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

- Pure types crate — no business logic, no storage interfaces
- Session IDs use UUID v4 (random, no ordering needed)

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `agent::session::SessionManager` owns session lifecycle logic; Router calls it; `AgentActor` holds the `Session` instance |
| `storage` | Defines `SessionStore` trait using session types; provides libsql implementation |
| `hook` | `SessionCreated` / `SessionDestroyed` hook points for welcome messages, audit, cleanup |
