# model - Shared Content Primitives

## Overview

`model` is Aura's lowest-level shared data crate. It provides only content representation types exchanged across modules and contains no business traits or error types.

Contents:

- **Content models**: `ContentBlock`, `BlobRef`, `ChatMessage`, `Role`, `MessageMetadata`
- **Memory types**: `MemoryEntry`, `MemoryCategory`
- **Governance types**: `TrustLevel`, `ArtifactSource`, `ExtensionManifest`, `ExtensionKind`

## Design Decisions

### Minimal scope

`model` retains only the content primitives that are genuinely used by both the channel layer and the LLM layer and cannot naturally belong to either. Session/user types live in `session`, message types in `channels`, operation types in `job`, and per-module error types replace any shared error enum. Governance types (`TrustLevel`, `ArtifactSource`) also live here as they are consumed by both `tools` and `skills`. Filesystem addresses (`WorkspacePaths`, `IdentityKind`, the workspace-relative filename constants, `AURA_CONFIG_PATH`) live in `aura-workspace::paths`, not here — they are workspace-shaped data, not content primitives.

### Media by reference, not inline

Multimedia content (`ContentBlock::Image/Audio/File`) uses `BlobRef` (id) instead of embedding raw binary data. This prevents `Session` and `Trace` from growing without bound, and allows media to live in separate object/blob storage.

### Thread safety

All `model` types are `Send + Sync + Serialize + Deserialize + Clone`.

### Memory types

`model` also houses the long-term memory domain types (`MemoryEntry`, `MemoryCategory`). These are pure data definitions consumed by `storage` (for `MemoryStore`) and `agent` (for `MemoryManager`). No memory-specific error type lives here — storage failures surface through `StorageError`, and business-level memory errors (embedding, dedup) are defined in `agent::memory`. Business logic (recall, store, dedup, expiration) lives in `agent::memory`.

#### Recall strategy

- **With embedder**: extract text → generate query vector → cosine similarity against stored embeddings → blend with importance → return top-N
- **Without embedder**: extract keywords → keyword search → sort by importance

Common post-processing: limit count to avoid context overflow, prioritize important memories, update `last_accessed` for recalled items.

#### Automatic memory storage

`maybe_store()` runs after the final response and decides whether the turn should become memory. Triggers: preference expressions, important facts, interaction length crossing summary threshold, heuristic rules. Memory itself does **not** call LLMs — rule-based defaults are used for importance scoring, upper layers may adjust before calling `store()`.

#### Deduplication

Check for semantically similar existing memories before inserting. Update the existing entry if similarity is high enough. Use vector similarity with embedder, text matching without.

#### Expiration management

Two dimensions:

- **Time-based**: `expires_at` computed from `auto_forget_days`; `forget_expired()` removes expired entries
- **Count-based**: `max_entries_per_user` limit; evict by lowest importance, then oldest `last_accessed`

Cleanup is triggered externally (cron); memory exposes methods but does not own a scheduler.

#### Memory categories

`User`, `Feedback`, `Project`, `Reference` — providing semantic categorization for retrieval and management. Pre-self_improvement rows used `UserPreference` / `KeyFact`; both deserialize to `User` via serde aliases. See [`self-improvement.md`](self-improvement.md) for the design rationale behind the four-category split.

#### Vector embeddings

Integrated through `rig::embeddings::EmbeddingModel`, injected by the `agent` assembly layer. Embeddings stored as `Vec<f32>` in `MemoryEntry.embedding`.

## Constraints

- `model` depends on no other workspace crate
- `model` does not define business interfaces or error types
- All upper layers use `model` only as a data exchange layer
- Any field that may enter logs, Trace, or Job should be sanitizable and serializable by default
- Memory context is positioned after System Prompt/Soul and before Compressed Summary in the context window

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `agent::memory::MemoryManager` owns recall/store/dedup logic; `AgentLoop` calls it; `EmbeddingModel` trait defined here |
| `storage` | Defines `MemoryStore` trait using memory types; provides libsql implementation |
| `workspace` | Complements with identity/strategy files (no overlap) |
| `context` | Memory context is injected into the context window by `agent` |
