# storage - Unified Storage Implementation Layer

## Overview

The `storage` crate implements the Store traits exposed by other modules (`SessionStore`, `TraceStore`, `SecretStore`, `JobStore`, `MemoryStore`, `CostStore`).

Its job is not domain modeling, but:

- Provide a unified backend factory (`StorageFactory`)
- Offer separate persistence implementations per domain
- Isolate SQLite and in-memory implementation details

## Design Decisions

### One struct per trait

Each domain has an independent Store implementation (e.g. `SqliteSessionStore`, `SqliteTraceStore`). They can share one `SqlitePool` while preserving domain isolation. No giant struct implementing every interface.

### StorageFactory solves initialization, not abstraction

At startup: `StorageFactory::create(&config)` → `StorageSet` (container holding all Store implementations). The `agent` layer injects individual stores into corresponding modules.

### Each Store manages its own tables

No cross-domain aggregate tables. Each Store manages its own schema, keeping domain boundaries clean.

### JSON field strategy

Fields difficult to fully structure (`SessionState.extra`, `Job.input/output`, `TraceSpan.input/result`) are stored as JSON. The security requirement still applies: these values must already be sanitized.

### In-memory backend

For unit tests, local development, and temporary runs. Does not promise data retention across restarts or full transaction semantics.

### Transaction boundaries

Use transactions for: `JobStore.update_status()` writing both `jobs` and `job_transitions`, `TraceStore.save_trace()` writing trace root and nodes, `SecretStore.store()` when replacing old values.

## Constraints

- Depends on all crates defining Store traits (`session`, `memory`, `trace`, `security`, `cost`, `job`) plus `core`
- Exposes trait objects externally, not concrete SQLite types
- Assumes upper layers have already sanitized data before persistence

## Collaboration

| Module | Role |
|--------|------|
| `session` / `memory` / `trace` / `security` / `cost` / `job` | Define Store traits that `storage` implements |
| `agent` | Injects all implementations through `StorageSet` |
