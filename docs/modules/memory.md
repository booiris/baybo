# memory - User-scoped Memory Store

## Overview

The `memory` crate owns a thin `MemoryManager` business-logic facade (list / search / store / delete / importance, with per-user eviction). The `MemoryStore` trait lives in the `aura-store` ports crate; domain types (`MemoryEntry`, `MemoryCategory`) live in `aura-model` so the wire shape is reusable from non-storage call sites.

`aura-storage` provides the libsql implementation of `MemoryStore` (the trait itself lives in `aura-store`), so downstream callers and tests can depend on `aura-memory` plus the ports crate for memory work.

There is currently **no automatic recall path and no auto-store path**. The agent loop does not consult the memory subsystem on incoming user content, and it does not write the assistant's response back to memory. The previous heuristic-based `recall` + `maybe_store` pipeline was removed because it pulled in arbitrary substrings, treated entire assistant outputs (including embedded code or document content) as memorable, and re-injected those snapshots as `Role::System` messages in subsequent turns. Any future memory mechanism must be driven by an explicit signal (a tool call, an operator action), not by substring matching against free-form text.

## Surface

`MemoryManager` exposes only operator-facing operations:

- `store(entry)` — persist a single `MemoryEntry`; runs per-user eviction afterwards.
- `list(user_id?)` — list entries, optionally scoped to one user.
- `search(user_id?, query, limit)` — substring search, scoped or global.
- `get(id)` / `delete(id)` — point lookup / point delete.
- `set_importance(id, importance)` — clamp `[0,1]` and persist.
- `delete_for_session(session_id)` — bulk delete entries tagged with `source_session_id`.

These power the gateway admin REST endpoints (`/v1/memory`) and any future operator-facing CLI surface. Nothing in the agent loop calls them.

## Constraints

- No dependency on `aura-storage` — the libsql impl converts its own errors at the trait boundary.
- `test_support::MemoryMemoryStore` is gated behind the `test-support` feature so it never ships in release builds. Downstream test crates pull it in via `aura-memory = { workspace = true, features = ["test-support"] }`.
- Per-user limit eviction (`enforce_user_limit`, default 1000 entries) runs after every successful write, scoring entries by `(importance, last_accessed)` ascending and dropping the lowest-ranked ones.

## Collaboration

| Module    | Role                                                                                                  |
| --------- | ----------------------------------------------------------------------------------------------------- |
| `model`   | Provides `MemoryEntry`, `MemoryCategory`                                                              |
| `gateway` | Wires `MemoryManager` into the admin REST surface (`/v1/memory` list / store / delete)                 |
| `store`   | Owns the `MemoryStore` trait contract and `StorageError`                                              |
| `storage` | Provides the libsql implementation of `MemoryStore` (trait from `aura-store`)                          |
