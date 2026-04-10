# Aura Module Documentation Index

Each document covers: module responsibilities, design decisions, key constraints, and collaboration with other modules.

## Reading Order

Bottom-up along the dependency graph:

1. [model.md](model.md) → [session.md](session.md) → [channels.md](channels.md)
2. [job.md](job.md) → [registry.md](registry.md) → [skills.md](skills.md)
3. [llm.md](llm.md) → [security.md](security.md) → [sandbox.md](sandbox.md)
4. [tools.md](tools.md) → [workspace.md](workspace.md) → [context.md](context.md)
5. [trace.md](trace.md) → [hook.md](hook.md)
6. [storage.md](storage.md) → [agent.md](agent.md)

## Module Groups

### Foundational Types Layer

- **model** — Shared content primitives (ChatMessage, ContentBlock, Role, BlobRef, MessageMetadata) and memory domain types (MemoryEntry, MemoryCategory). No business traits.

### Ingress and Security Boundary Layer

- **session** — Session domain types (User, ChannelType, Session, SessionState) and error definitions.
- **channels** — Channel adapter trait and shared message types (Message, IncomingMessage, OutgoingMessage). Concrete adapters are WASM modules under `channels/`.
- **security** — Cryptographic primitives (EncryptionKey, encrypt/decrypt), leak detection (LeakDetector), error types.

### Capability and Governance Layer

- **llm** — LLM provider wrapping and response parsing.
- **sandbox** — Execution isolation (WASM + container), including WasmRuntime subcomponent.
- **tools** — Tool abstraction, registration, capability declarations, runtime routing.
- **registry** — Extension artifact verification and installation governance. Owns TrustLevel, ArtifactSource.
- **skills** — Declarative skill definitions, selection, trust tiers, hot reload.
- **workspace** — Identity files, heartbeat, and routine configuration.
- **context** — Context appending, compression, snapshots, restoration.

### Runtime and Observability Layer

- **trace** — Trace domain types (SessionTrace, TraceNode, SpanHandle) and tree/fork/snapshot utilities.
- **job** — Job domain types (Job, JobStatus, JobTransition) and state machine. Owns OperationKind.
- **hook** — Lifecycle extension points.

### Infrastructure and Assembly Layer

- **storage** — Defines all Store traits (`SessionStore`, `MemoryStore`, `TraceStore`, `SecretStore`, `JobStore`, `CostStore`); implements all via libsql (single backend).
- **agent** — Assembly layer: Actor, AgentLoop, ToolExecutor, ObservabilityRecorder, cost management (CostTracker, CostGuard), plus all domain managers (SessionManager, MemoryManager, TraceCollector, JobManager, SecretVault, SecurityGateway).
## Dependency Overview

```
model
  ├── session ──► model
  ├── channels ──► model, session
  ├── llm ──► model
  ├── context ──► model, session
  ├── security ──► model, session, channels
  ├── hook ──► channels
  ├── trace ──► model, context, job
  ├── tools ──► model, session, registry, sandbox
  ├── skills ──► registry
  └── job (no internal deps)
  └── registry (no internal deps)
  └── workspace (no internal deps)
  └── sandbox (no internal deps)

storage ──► model, session, trace, security, job (defines all Store traits; sole backend: libsql)
agent   ──► model, llm, tools, workspace, context, session, trace, job, security, storage, hook, sandbox, channels, registry
```

## Key Constraints

- Each module defines its own error type; no shared error enum
- Store traits defined in `storage`; domain types in their own crates; business logic in `agent`; `model` contains shared content primitives and memory domain types
- Logs, Trace, and Job must not record sensitive plaintext — only placeholders or sanitized summaries
- Tool/skill extensions must carry source, version, hash, trust level, and capability declarations
- High-risk execution must be upgraded to the container surface in `sandbox`
- The Job state machine is fixed: `Pending → InProgress → Completed → Submitted → Accepted` (with `Failed` and `Stuck` branches)
- Multimedia passed by reference — no raw binary in sessions, snapshots, or Trace
- Hot reload, tool updates, identity changes, and config changes must leave provenance records in Trace
- Heartbeat, routine, cron, and background execution must all enter Job and Trace
