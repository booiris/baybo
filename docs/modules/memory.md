# memory - Long-Term Memory Types

## Overview

The `memory` crate defines domain types for long-term user memory (`MemoryEntry`, `MemoryCategory`) and the `MemoryError` error type.

Business logic (`MemoryManager`, `EmbeddingModel` trait, recall/store/dedup) lives in `agent::memory`. The `MemoryStore` trait is defined in `storage::memory`.

**Position**: `agent::memory::MemoryManager` is used by `AgentLoop`. Each turn, Agent Loop calls `recall()` before building context and `maybe_store()` after producing the reply.

## Design Decisions

### Recall strategy

- **With embedder**: extract text → generate query vector → cosine similarity against stored embeddings → blend with importance → return top-N
- **Without embedder**: extract keywords → keyword search → sort by importance

Common post-processing: limit count to avoid context overflow, prioritize important memories, update `last_accessed` for recalled items.

### Automatic memory storage

`maybe_store()` runs after the final response and decides whether the turn should become memory. Triggers: preference expressions, important facts, interaction length crossing summary threshold, heuristic rules. Memory itself does **not** call LLMs — rule-based defaults are used for importance scoring, upper layers may adjust before calling `store()`.

### Deduplication

Check for semantically similar existing memories before inserting. Update the existing entry if similarity is high enough. Use vector similarity with embedder, text matching without.

### Expiration management

Two dimensions:

- **Time-based**: `expires_at` computed from `auto_forget_days`; `forget_expired()` removes expired entries
- **Count-based**: `max_entries_per_user` limit; evict by lowest importance, then oldest `last_accessed`

Cleanup is triggered externally (cron, heartbeat); memory exposes methods but does not own a scheduler.

### Memory categories

`UserPreference`, `KeyFact`, `InteractionSummary`, `Custom(String)` — providing semantic categorization for retrieval and management.

### Vector embeddings

Integrated through `rig::embeddings::EmbeddingModel`, injected by the `agent` assembly layer. Embeddings stored as `Vec<f32>` in `MemoryEntry.embedding`.

## Constraints

- Pure types crate — no business logic, no storage interfaces
- Does **not** depend on `llm`, `agent`, `storage`, or `workspace`
- Memory context is positioned after System Prompt/Soul and before Compressed Summary in the context window

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `agent::memory::MemoryManager` owns recall/store/dedup logic; `AgentLoop` calls it; `EmbeddingModel` trait defined here |
| `storage` | Defines `MemoryStore` trait using memory types; provides libsql implementation |
| `workspace` | Complements with identity/strategy files (no overlap) |
| `context` | Memory context is injected into the context window by `agent` |
