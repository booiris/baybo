# model - Shared Content Primitives

## Overview

`model` is Aura's lowest-level shared data crate. It provides only content representation types exchanged across modules and contains no business traits or error types.

Contents:

- **Content models**: `ContentBlock`, `BlobRef`, `ChatMessage`, `Role`, `MessageMetadata`
- **Session types**: `Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `TriggerKind`, `SystemReason`, `Lineage`, `LineageKind`
- **Memory types**: `MemoryEntry`, `MemoryCategory`
- **Governance types**: `TrustLevel`, `ArtifactSource`, `ExtensionManifest`, `ExtensionKind`

## Design Decisions

### Minimal scope

`model` retains only the content primitives that are genuinely used by both the channel layer and the LLM layer and cannot naturally belong to either. Session/user domain types (`Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `TriggerKind`, `SystemReason`, `Lineage`, `LineageKind`) live here so every consumer (channels, agent, storage, session manager) shares one shape; `aura-session` re-uses them via `aura_model` and adds only the lifecycle manager + error type. Message types live in `channels`, operation types in `job`, and per-module error types replace any shared error enum. Governance types (`TrustLevel`, `ArtifactSource`) also live here as they are consumed by both `tools` and `skills`. Filesystem addresses (`WorkspacePaths`, `IdentityKind`, the workspace-relative filename constants, `AURA_CONFIG_PATH`) live in `aura-workspace::paths`, not here — they are workspace-shaped data, not content primitives.

### Media by reference, not inline

Multimedia content (`ContentBlock::Image/Audio/File`) uses `BlobRef` (id) instead of embedding raw binary data. This prevents `Session` and `Trace` from growing without bound, and allows media to live in separate object/blob storage.

### Thread safety

All `model` types are `Send + Sync + Serialize + Deserialize + Clone`.

### Memory types

`model` houses the memory domain types (`MemoryEntry`, `MemoryCategory`). These are pure data definitions consumed by `storage` (for `MemoryStore`) and `aura-memory` (for `MemoryManager`). Storage failures surface through `StorageError`; `MemoryManager` only exposes operator-facing CRUD (list / search / store / delete / importance) and per-user eviction — there is no automatic recall or auto-store path.

#### Eviction

`MemoryEntry` carries an `expires_at: Option<DateTime<Utc>>` slot, but no time-based sweeper is wired today. The active policy is count-based: `MemoryManager::enforce_user_limit` clamps each user to `max_entries_per_user` (default 1000), evicting by lowest importance and then oldest `last_accessed`. Eviction runs after every `store()`.

#### Memory categories

`UserPreference`, `KeyFact` — left in place as categorisation hints for operator-stored entries.

## Constraints

- `model` depends on no other workspace crate
- `model` does not define business interfaces or error types
- All upper layers use `model` only as a data exchange layer
- Any field that may enter logs, Trace, or Job should be sanitizable and serializable by default

## Collaboration

| Module | Role |
|--------|------|
| `memory` | Owns the `MemoryManager` facade (list/search/store/delete/importance, per-user eviction); consumed by the admin REST surface. The `MemoryStore` trait lives in `aura-store` |
| `storage` | Provides the libsql implementation of `MemoryStore` (trait from `aura-store`) |
| `workspace` | Complements with identity/strategy files (no overlap) |
