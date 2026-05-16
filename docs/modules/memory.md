# memory - Long-term User Memory

## Overview

The `memory` crate is the complete home for long-term, user-scoped memory: the `MemoryStore` trait and the `MemoryManager` business-logic facade that wraps it. Domain types (`MemoryEntry`, `MemoryCategory`) live in `aura-model` so the wire shape is reusable from non-storage call sites.

`aura-storage` provides the libsql implementation of `MemoryStore`; the trait itself lives here so downstream callers and tests can depend on `aura-memory` alone for memory work.

## Design Decisions

### MemoryManager owns recall, dedup, importance, and eviction

`MemoryManager` is the sole entry point for production code. It composes the raw `MemoryStore` with three pieces of business logic:

1. **Recall** — when the agent loop has the user's incoming `ContentBlock`s, `recall()` extracts text, runs an embedding-aware similarity scan (or a plain substring scan when no embedder is configured), and returns the top-`DEFAULT_RECALL_LIMIT` matches sorted by combined similarity + importance.
2. **Dedup at write time** — `maybe_store()` watches the user's response for preference / fact indicators (`"i prefer"`, `"my project"`, …) and calls `store_with_dedup()` so two near-identical entries aren't both kept. The dedup threshold (`DEDUP_SIMILARITY_THRESHOLD = 0.85` cosine when an embedder is wired up; exact-string match otherwise) is deliberately tighter than the recall threshold.
3. **Per-user eviction** — `enforce_user_limit()` runs after every store, scoring entries by (`importance`, `last_accessed`) ascending and dropping the lowest-ranked ones once `max_entries_per_user` is exceeded.

### Embedding model is optional

`EmbeddingModel` is a trait (`async fn embed(text) -> Vec<f32>`) injected at construction. When `None`, recall falls back to the store's substring search and dedup falls back to exact-string match. Production deployments wire an embedding provider in via the LLM client pool; tests use `without_embedder()` so they don't need a model fixture.

### `MemoryError` mirrors `JobError`

`MemoryError` carries `Embedding(String)`, `NotFound(String)`, `Storage(String)`, and `Internal(anyhow::Error)`. The `Storage` variant is a stringified `aura_storage::StorageError` produced at the libsql impl boundary — this keeps `aura-memory` free of any `aura-storage` dependency.

## Constraints

- No dependency on `aura-storage` — the libsql impl converts its own errors at the trait boundary
- `test_support::MemoryMemoryStore` is gated behind the `test-support` feature so it never ships in release builds. Downstream test crates pull it in via `aura-memory = { workspace = true, features = ["test-support"] }`
- Per-user limit eviction runs after every successful write; `recall` does not trigger eviction

## Collaboration

| Module    | Role                                                                                                                |
| --------- | ------------------------------------------------------------------------------------------------------------------- |
| `model`   | Provides `MemoryEntry`, `MemoryCategory`, `ContentBlock`, `Session` (used by `maybe_store` for the user-id scope)    |
| `agent`   | Constructs one `MemoryManager` and shares it with `AgentLoop`; calls `recall`/`maybe_store` around each turn         |
| `storage` | Provides the libsql implementation of `MemoryStore`; the trait itself lives in this crate                            |
