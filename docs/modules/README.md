# Aura Module Documentation Index

Each document covers: module responsibilities, design decisions, key constraints, and collaboration with other modules.

## Reading Order

Bottom-up along the dependency graph:

1. [model.md](model.md) → [config.md](config.md) → [session.md](session.md) → [channels.md](channels.md)
2. [job.md](job.md) → [cron.md](cron.md) → [registry.md](registry.md) → [skills.md](skills.md)
3. [llm.md](llm.md) → [security.md](security.md) → [sandbox.md](sandbox.md)
4. [tools.md](tools.md) → [workspace.md](workspace.md) → [context.md](context.md)
5. [trace.md](trace.md) → [hook.md](hook.md)
6. [storage.md](storage.md) → [agent.md](agent.md) → [bootstrap.md](bootstrap.md) → [cli.md](cli.md)

## Module Groups

### Foundational Types Layer

- **model** — Shared content primitives (ChatMessage, ContentBlock, Role, BlobRef, MessageMetadata) and memory domain types (MemoryEntry, MemoryCategory). No business traits.
- **config** — Root `AuraConfig` with JSON loading and `validate()`. Ten sections (llm, agent, session, channels, sandbox, security, tools, trace, cost, workspace). Uses mirror structs to stay decoupled from domain crates.

### Ingress and Security Boundary Layer

- **session** — Session domain types (User, ChannelType, Session, SessionState) and error definitions.
- **channels** — Channel adapter trait, shared message types (Message, IncomingMessage, OutgoingMessage), and `ChannelRegistry`. Includes the built-in `TuiAdapter` (Ratatui terminal UI, see [`tui.md`](./tui.md)); additional adapters can be WASM modules loaded at runtime.
- **security** — Cryptographic primitives (EncryptionKey, encrypt/decrypt), leak detection (LeakDetector), error types.

### Capability and Governance Layer

- **llm** — LLM provider wrapping and response parsing.
- **sandbox** — Execution isolation (WASM + container), including WasmRuntime subcomponent.
- **tools** — Tool abstraction, registration, capability declarations, runtime routing. MCP client support via `rmcp`.
- **registry** — Extension artifact verification and installation governance. Owns TrustLevel, ArtifactSource.
- **skills** — Declarative skill definitions, selection, trust tiers, hot reload.
- **workspace** — Identity files and long-running configuration.
- **cron** — Cron job domain types (`CronJob`, `CronExecution`, `CronStatus`, `CronRunMode`, `CronError`). Standard cron syntax.
- **context** — Context appending, compression, snapshots, restoration.

### Runtime and Observability Layer

- **trace** — Trace domain types (SessionTrace, TraceNode, SpanHandle) and tree/fork/snapshot utilities.
- **job** — Job domain types (Job, JobStatus, JobTransition) and state machine. Owns OperationKind.
- **hook** — Lifecycle extension points.

### Infrastructure and Assembly Layer

- **storage** — Defines all Store traits (`SessionStore`, `MemoryStore`, `TraceStore`, `SecretStore`, `JobStore`, `CostStore`, `CronStore`); implements all via libsql (single backend). `CronStore` uses opaque row types (`CronJobRow`, `CronExecutionRow`) — no dependency on `cron` domain crate.
- **agent** — Assembly layer: Actor, AgentLoop, ToolExecutor, ObservabilityRecorder, cost management (CostTracker, CostGuard), plus all domain managers (SessionManager, MemoryManager, TraceCollector, JobManager, SecretVault, SecurityGateway, CronScheduler). Bridges cron domain types and storage row types.
- **bootstrap** — Binary entry point (`src/main.rs`) and `boot` submodule. Loads `AuraConfig`, translates each section into domain types, and wires the Arc graph that `agent` consumes. Unit-tested mappings live in `boot`; Arc lifetime management stays in `main.rs`.
- **cli** — Operator-facing command layer (`aura-cli`). One `clap` tree drives both argv-mode commands (`aura config show`) and in-conversation slash commands (`/config show`). Read-only and mutating commands share a single dispatcher; slash input never enters the agent's context.

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
  ├── tools ──► model, session, registry, sandbox, rmcp
  ├── skills ──► registry
  └── job (no internal deps)
  └── registry (no internal deps)
  └── workspace (no internal deps)
  └── sandbox (no internal deps)
  └── config (no internal deps; external only)

storage   ──► model, session, trace, security, job (defines all Store traits; sole backend: libsql; CronStore uses opaque row types)
agent     ──► model, llm, tools, workspace, context, session, trace, job, cron, security, storage, hook, sandbox, channels, registry, config
bootstrap ──► config + all domain crates it assembles (entry point only)
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
- Cron and background execution must all enter Job and Trace
