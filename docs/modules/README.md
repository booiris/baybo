# Aura Module Documentation Index

Each document covers: module responsibilities, design decisions, key constraints, and collaboration with other modules.

## Reading Order

Bottom-up along the dependency graph:

1. [model.md](model.md) → [session.md](session.md) → [channels.md](channels.md)
2. [job.md](job.md) → [registry.md](registry.md) → [skills.md](skills.md)
3. [llm.md](llm.md) → [security.md](security.md) → [sandbox.md](sandbox.md)
4. [tools.md](tools.md) → [memory.md](memory.md) → [workspace.md](workspace.md) → [context.md](context.md)
5. [trace.md](trace.md) → [cost.md](cost.md) → [hook.md](hook.md)
6. [storage.md](storage.md) → [agent.md](agent.md) → [wasm-runtime.md](wasm-runtime.md)

## Module Groups

### Foundational Types Layer

- **model** — Shared content primitives (ChatMessage, ContentBlock, Role, BlobRef, MessageMetadata). No business traits.

### Ingress and Security Boundary Layer

- **session** — Session lifecycle, storage interfaces, and shared identity types (User, ChannelType, Session, SessionState).
- **channels** — Multi-channel message ingress and delivery. Owns Message, IncomingMessage, OutgoingMessage.
- **security** — Input sanitization, secret management, output re-sanitization, network policy decisions.

### Capability and Governance Layer

- **llm** — LLM provider wrapping and response parsing.
- **sandbox** — Execution isolation (WASM + container).
- **tools** — Tool abstraction, registration, capability declarations, runtime routing.
- **registry** — Extension artifact verification and installation governance. Owns TrustLevel, ArtifactSource.
- **skills** — Declarative skill definitions, selection, trust tiers, hot reload.
- **memory** — Long-term memory storage and recall.
- **workspace** — Identity files, heartbeat, and routine configuration.
- **context** — Context appending, compression, snapshots, restoration.

### Runtime and Observability Layer

- **trace** — Call chains, snapshot rollback, provenance.
- **job** — Task state machine and state history. Owns OperationKind.
- **cost** — Token usage records and spending guards.
- **hook** — Lifecycle extension points.

### Infrastructure and Assembly Layer

- **storage** — Backend implementations of Store traits.
- **agent** — Assembly layer: Actor, AgentLoop, ToolExecutor, ObservabilityRecorder.
- **wasm-runtime** — WasmRuntime subcomponent details (inside sandbox).

## Dependency Overview

```
model
  ├── session ──► model
  ├── channels ──► model, session
  ├── llm ──► model
  ├── context ──► model, session
  ├── memory ──► model
  ├── security ──► model, session, channels
  ├── hook ──► channels
  ├── trace ──► model, context, job
  ├── tools ──► model, session, registry, sandbox
  ├── skills ──► registry
  └── job (no internal deps)
  └── registry (no internal deps)
  └── cost (no internal deps)
  └── workspace (no internal deps)
  └── sandbox (no internal deps)

storage ──► model, session, channels, memory, trace, security, cost, job, registry
agent   ──► model, llm, tools, memory, workspace, context, session, trace, job, security, cost, hook, sandbox, channels, registry
```

## Key Constraints

- Each module defines its own error type; no shared error enum
- Traits defined in their own modules; `model` only contains shared content primitives
- Logs, Trace, and Job must not record sensitive plaintext — only placeholders or sanitized summaries
- Tool/skill extensions must carry source, version, hash, trust level, and capability declarations
- High-risk execution must be upgraded to the container surface in `sandbox`
- The Job state machine is fixed: `Pending → InProgress → Completed → Submitted → Accepted` (with `Failed` and `Stuck` branches)
- Multimedia passed by reference — no raw binary in sessions, snapshots, or Trace
- Hot reload, tool updates, identity changes, and config changes must leave provenance records in Trace
- Heartbeat, routine, cron, and background execution must all enter Job and Trace
