# memory - Long-Term Memory System

## Overview

The `memory` crate manages the full lifecycle of long-term user memory: storage, retrieval, semantic search, and expiration cleanup.

Core responsibilities:

- Store important memories (preferences, facts, summaries) produced during interaction
- Recall relevant historical memories and inject as context enhancement
- Support vector-embedding semantic search (fallback to keyword matching without embedder)
- Decide automatically whether to store, avoiding redundancy
- Enforce expiration cleanup and per-user memory-count limits

**Position**: used by `agent::AgentLoop`. Each turn, Agent Loop calls `recall()` before building context and `maybe_store()` after producing the reply.

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

- Depends only on `core` (plus `rig` for embeddings)
- Does **not** depend on `llm`, `agent`, `storage`, or `workspace`
- `MemoryStore` trait defined here; implementations live in `storage`
- Memory context is positioned after System Prompt/Soul and before Compressed Summary in the context window

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` calls `recall()` and `maybe_store()`, injects embedder |
| `storage` | Provides `MemoryStore` implementations (SQLite, in-memory) |
| `workspace` | Complements with identity/strategy files (no overlap) |
| `context` | Memory context is injected into the context window by `agent` |
