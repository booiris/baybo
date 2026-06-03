# model - Shared Content Primitives

## Overview

`model` is Aura's lowest-level shared data crate. It provides the data types — content representations, shared domain records, ID newtypes, and protocol shapes — exchanged across modules, and contains no business traits or error types (each is a plain data definition consumed by higher layers).

Contents:

- **Content models**: `ContentBlock`, `BlobRef`, `ChatMessage`, `Role`, `MessageSource`, `ThinkingContent`, `MessageMetadata` (now an empty struct), plus the `TOOL_OUTPUT_OPEN_PREFIX` / `TOOL_OUTPUT_CLOSE_PREFIX` marker constants
- **Session types**: `Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `TriggerKind`, `Lineage`, `LineageKind`, `BackgroundCompressionPayload`
- **Memory types**: `MessageSource::RecalledMemory` (the framed recall-injection marker) + the `ChatMessage::recalled_memory` constructor
- **Governance types**: `TrustLevel`, `ArtifactSource`, `ExtensionManifest`, `ExtensionKind`
- **Cost & money types**: `CostRecord`, `CostSummary`, `TimeRange`, `MicroUsd` (integer micro-USD; the project never uses floats for money)
- **Cron types**: `CronJob`, `CronExecution`, `CronSchedule`, `CronStatus`, `ExecutionStatus`
- **Approval types**: `ApprovalDecision`, `ApprovedResource`, `HostPattern`, `ResourceAccess`
- **Subagent spawn protocol** (`spawn_protocol`): `SubagentSpawnRequest`, `SubagentResult`, `PendingSubagentResult`, `SubagentReturn`, `SubagentExitStatus`, the `SPAWN_SUBAGENT_TOOL_NAME` const, and related markers
- **External-agent types**: `ExternalAgentKind`, `SubagentBackend`, `SubagentBackendKind`
- **LLM routing types**: `LlmEntryName`, `ModelTier`, `LlmPricingOverride`
- **ID newtypes**: `SessionId`, `JobId`, `SpanId`, `StepId`, `CostRecordId`, `ParallelGroup`

## Design Decisions

### Minimal scope

`model` retains the data types that are shared by two or more layers (channel, LLM, storage, agent) and cannot naturally belong to any single one — content primitives, cross-cutting domain records, ID newtypes, and protocol shapes. Session/user domain types (`Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `TriggerKind`, `Lineage`, `LineageKind`) live here so every consumer (channels, agent, storage, session manager) shares one shape; `aura-session` re-uses them via `aura_model` and adds only the lifecycle manager + error type. Message types live in `channels`, operation types in `job`, and per-module error types replace any shared error enum. Governance types (`TrustLevel`, `ArtifactSource`) also live here as they are consumed by both `tools` and `skills`. Filesystem addresses (`WorkspacePaths`, `IdentityKind`, the workspace-relative filename constants, `AURA_CONFIG_PATH`) live in `aura-workspace::paths`, not here — they are workspace-shaped data, not content primitives.

### Media by reference, not inline

Multimedia content (`ContentBlock::Image/Audio/File`) uses `BlobRef` (id) instead of embedding raw binary data. This prevents `Session` and `Trace` from growing without bound, and allows media to live in separate object/blob storage.

### Thread safety

All `model` types are `Send + Sync + Serialize + Deserialize + Clone`.

### Memory types

`model` no longer houses memory domain records — the old `MemoryEntry` / `MemoryCategory` CRUD types were removed with the `MemoryManager` facade. What `model` contributes to the new pluggable `Memory` trait (see [`memory.md`](memory.md)) is the recall-injection marker: the `MessageSource::RecalledMemory` variant plus the `ChatMessage::recalled_memory` constructor, so recalled memories ride the transcript as a framed, persisted block rather than a `Role::System` message. The trait, its value types (`MemoryContext`, `RecalledMemory`), and any backend storage live in `aura-memory`, not here.

## Constraints

- `model` depends on no other workspace crate
- `model` does not define business interfaces or error types
- All upper layers use `model` only as a data exchange layer
- Any field that may enter logs, Trace, or Job should be sanitizable and serializable by default

## Collaboration

| Module | Role |
|--------|------|
| `memory` | Owns the pluggable `Memory` trait + `NoopMemory`; consumes `MessageSource::RecalledMemory` / `ChatMessage::recalled_memory` for framed recall injection |
| `workspace` | Complements with identity/strategy files (no overlap) |
