# model - Shared Content Primitives

## Overview

`model` is Baybo's lowest-level shared data crate. It provides the data types — content representations, shared domain records, ID newtypes, and protocol shapes — exchanged across modules, and contains no business traits or error types (each is a plain data definition consumed by higher layers).

Contents:

- **Content models**: `ContentBlock`, `BlobRef`, `ChatMessage` (including the persisted `platform_msg_id` idempotency key for channel-originated user rows), `Role`, `MessageSource`, `ThinkingContent`, `MessageMetadata` (now an empty struct), `ToolResultMeta`, plus the `TOOL_OUTPUT_OPEN_PREFIX` / `TOOL_OUTPUT_CLOSE_PREFIX` marker constants and the `SHA256_PREFIX` blob-id prefix with its `blob_content_digest` helper
- **Session types**: `Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `TriggerKind`, `Lineage`, `LineageKind`, `BackgroundCompressionPayload`, `BackgroundNotificationState`, `BackgroundNotificationGroup`, `BackgroundNotificationDelivery`, `MAX_SESSION_TITLE_LEN`
- **Memory types**: `MessageSource::RecalledMemory` (the framed recall-injection marker) + the `ChatMessage::recalled_memory` constructor
- **Task types** (`task`): `Task`, `TaskStatus`, the `TASK_CREATE_TOOL_NAME` / `TASK_GET_TOOL_NAME` / `TASK_LIST_TOOL_NAME` / `TASK_UPDATE_TOOL_NAME` consts, and `TASK_MUTATING_TOOL_NAMES`
- **Control events**: `ControlEvent`, `ControlEventKind`
- **Folder types**: `FolderId`, `FolderSummary`, `MAX_FOLDER_NAME_LEN`
- **Agent-profile types**: `AgentProfileId`, `AgentFramework`, `BUILTIN_AGENT_PROFILE_ID`, `MAX_AGENT_PROFILE_NAME_CHARS`
- **Governance types**: `TrustLevel`, `ArtifactSource`
- **Security types**: `PlaceholderId`, `SecretKind`
- **Cost & money types**: `CostRecord`, `CostSummary`, `CallReason`, `TimeRange`, `MicroUsd` (integer micro-USD; the project never uses floats for money)
- **MCP authorization types**: `McpTransportIdentity` (validated versioned SHA-256 string) and `McpToolGrant` (exact namespaced tool + transport identity); cron jobs and executions persist these without depending on the tools runtime
- **Cron types**: `CronJob`, `CronExecution`, `CronSchedule`, `CronStatus`, `ExecutionStatus`
- **Approval types**: `ApprovalDecision`, `ApprovedResource`, `HostPattern`, `ResourceAccess`
- **Subagent spawn protocol** (`spawn_protocol`): `SubagentSpawnRequest`, `SubagentParentContext`, `SubagentResult`, `SubagentExitStatus`, `PendingBackgroundResult`, `BackgroundJobKind`, `OnTimeout`, the `SPAWN_SUBAGENT_TOOL_NAME` const, and related markers (`SUBAGENT_CHANNEL_TAG`, `BACKGROUND_SUBAGENT_HANDLE_PREFIX`, `BACKGROUND_DISPATCH_ACK_PREFIX`, `new_background_handle`)
- **External-agent types**: `ExternalAgentKind`, `SubagentBackend`, `SubagentBackendKind`, `SubagentBackendTag`, the `BAYBO_BACKEND_TAG` const
- **LLM routing types**: `LlmEntryName`, `ModelTier`, `LlmPricingOverride`
- **Fingerprints**: `FileFingerprint` (mtime + size, used by the read-before-write tracker)
- **ID newtypes**: `SessionId`, `TurnId`, `SpanId`, `StepId`, `CostRecordId`, `TaskId`, `ParallelGroup`

## Design Decisions

### The `<tool_output>` envelope lives here

`wrap_tool_output` sits beside the `TOOL_OUTPUT_{OPEN,CLOSE}_PREFIX` constants
it keys off, so the wrapper and the literals cannot drift apart, and so every
crate that feeds untrusted text to a model can reach it: `baybo-context` for the
main transcript, `baybo-tools` for the out-of-band bash risk-judge prompts.
`baybo-context` already depends on `baybo-tools`, so the reverse would be a
cycle — this crate is the only common ancestor. Detection and secret
sanitization stay in `baybo-security`; only the format lives here, which is why
injection-marker rule names arrive as plain strings rather than as a
`baybo-security` type.

### Minimal scope

`model` retains the data types that are shared by two or more layers (channel, LLM, storage, agent) and cannot naturally belong to any single one — content primitives, cross-cutting domain records, ID newtypes, and protocol shapes. Session/user domain types (`Session`, `User`, `ChannelType`, `SessionState`, `TriggerSource`, `TriggerKind`, `Lineage`, `LineageKind`) live here so every consumer (channels, agent, storage, session manager) shares one shape; `baybo-session` re-uses them via `baybo_model` and adds only the lifecycle manager + error type. Message types live in `channels`, operation types in `turn`, and per-module error types replace any shared error enum. Governance types (`TrustLevel`, `ArtifactSource`) also live here as they are consumed by both `tools` and `skills`. Filesystem addresses (`WorkspacePaths`, `IdentityKind`, the workspace-relative filename constants, `BAYBO_CONFIG_PATH`) live in `baybo-workspace::paths`, not here — they are workspace-shaped data, not content primitives.

### Media by reference, not inline

Multimedia content (`ContentBlock::Image/Audio/File`) uses `BlobRef` (id) instead of embedding raw binary data. This prevents `Session` and `Trace` from growing without bound, and allows media to live in separate object/blob storage.

### Thread safety

All `model` types are `Send + Sync + Clone`; persisted and wire-visible types are additionally `Serialize + Deserialize` (the in-process spawn-handoff types `SubagentResult` and `SubagentParentContext` — which carries a `CancellationToken` — are not).

### Memory types

`model` no longer houses memory domain records — the old `MemoryEntry` / `MemoryCategory` CRUD types were removed with the `MemoryManager` facade. What `model` contributes to the new pluggable `Memory` trait (see [`memory.md`](memory.md)) is the recall-injection marker: the `MessageSource::RecalledMemory` variant plus the `ChatMessage::recalled_memory` constructor, so recalled memories ride the transcript as a framed, persisted block rather than a `Role::System` message. The trait, its value types (`MemoryContext`, `RecalledMemory`), and any backend storage live in `baybo-memory`, not here.

## Constraints

- `model` depends on no other workspace crate
- `model` does not define business interfaces or error types
- All upper layers use `model` only as a data exchange layer
- Any field that may enter logs, Trace, or Turn should be sanitizable and serializable by default

## Collaboration

| Module | Role |
|--------|------|
| `memory` | Owns the pluggable `Memory` trait + `NoopMemory`; consumes `MessageSource::RecalledMemory` / `ChatMessage::recalled_memory` for framed recall injection |
| `workspace` | Complements with identity/strategy files (no overlap) |
