# model - Shared Content Primitives

## Overview

`model` is Aura's lowest-level shared data crate. It provides only content representation types exchanged across modules and contains no business traits or error types.

Contents:

- **Content models**: `ContentBlock`, `BlobRef`, `ChatMessage`, `Role`, `MessageMetadata`

## Design Decisions

### Minimal scope

Previous `core` crate was split: session/user types moved to `session`, message types to `channels`, operation types to `job`, governance types to `registry`, and per-module error types replaced the shared `AuraError`. `model` retains only the content primitives that are genuinely used by both the channel layer and the LLM layer and cannot naturally belong to either.

### Media by reference, not inline

Multimedia content (`ContentBlock::Image/Audio/File`) uses `BlobRef` (id) instead of embedding raw binary data. This prevents `Session`, `ContextSnapshot`, and `Trace` from growing without bound, and allows media to live in separate object/blob storage.

### Thread safety

All `model` types are `Send + Sync + Serialize + Deserialize + Clone`.

## Constraints

- `model` depends on no other workspace crate
- `model` does not define business interfaces or error types
- All upper layers use `model` only as a data exchange layer
- Any field that may enter logs, Trace, or Job should be sanitizable and serializable by default
