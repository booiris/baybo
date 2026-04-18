# Aura Module Documentation Index

Each document covers: module responsibilities, design decisions, key constraints, and collaboration with other modules.

## Reading Order

Bottom-up along the dependency graph:

1. [model.md](model.md) → [config.md](config.md) → [session.md](session.md) → [channels.md](channels.md)
2. [job.md](job.md) → [cron.md](cron.md) → [skills.md](skills.md)
3. [llm.md](llm.md) → [security.md](security.md)
4. [tools.md](tools.md) → [workspace.md](workspace.md) → [context.md](context.md)
5. [trace.md](trace.md) → [hook.md](hook.md)
6. [storage.md](storage.md) → [agent.md](agent.md) → [bootstrap.md](bootstrap.md) → [cli.md](cli.md)

## Module Groups

### Foundational Types Layer

- **model** — Shared content primitives (ChatMessage, ContentBlock, Role, BlobRef, MessageMetadata), memory domain types (MemoryEntry, MemoryCategory), and governance types (TrustLevel, ArtifactSource, ExtensionManifest). No business traits.
- **config** — Root `AuraConfig` with JSON loading and `validate()`. Sections (llm, agent, session, channels, security, tools, trace, cost, workspace). Uses mirror structs to stay decoupled from domain crates.

### Ingress and Security Boundary Layer

- **session** — `SessionError` and `SessionManager` (lifecycle logic). Session domain types (`User`, `ChannelType`, `Session`, `SessionState`) live in `model`; the `SessionStore` trait lives in `storage`. `aura-session` depends on both.
- **channels** — Channel adapter trait, shared message types (Message, IncomingMessage, OutgoingMessage), and `ChannelRegistry`. Includes the built-in `TuiAdapter` (Ratatui terminal UI, see [`tui.md`](./tui.md)).
- **security** — Cryptographic primitives (EncryptionKey, encrypt/decrypt), leak detection (LeakDetector), error types.

### Capability and Governance Layer

- **llm** — LLM provider wrapping and response parsing.
- **tools** — Tool abstraction, registration, capability declarations, runtime routing. (MCP client support is temporarily removed; see `docs/todo/reintroduce-mcp-support.md`.)
- **skills** — Declarative skill definitions, selection, trust tiers, hot reload.
- **[skills-assessor](skills-assessor.md)** — LLM-backed risk classifier for skills. Hashes the skill directory, caches verdicts (`Safe`/`Suspicious`/`Dangerous`) in `SkillRiskStore`, tiers large skills (primary-scope synchronous + full-scope background worker with restart-safe job recovery), and gates skill injection in `AgentLoop` so only `Dangerous` blocks. Kept separate from `skills` so selection stays deterministic and offline-capable.
- **workspace** — Identity files and long-running configuration.
- **cron** — Cron job domain types (`CronJob`, `CronExecution`, `CronStatus`, `CronRunMode`, `CronError`). Standard cron syntax.
- **context** — Context appending, compression, snapshots, restoration.

### Runtime and Observability Layer

- **trace** — Trace domain types (SessionTrace, TraceNode, SpanHandle) and tree/fork/snapshot utilities.
- **job** — Job domain types (Job, JobStatus, JobTransition) and state machine. Owns OperationKind.
- **hook** — Lifecycle extension points.

### Infrastructure and Assembly Layer

- **storage** — Defines all Store traits (`SessionStore`, `MemoryStore`, `TraceStore`, `SecretStore`, `JobStore`, `CostStore`, `CronStore`, `SkillRiskStore`); implements all via libsql (single backend). `CronStore` uses opaque row types (`CronJobRow`, `CronExecutionRow`) — no dependency on `cron` domain crate. `SkillRiskStore` defines its own `RiskVerdict` / `RiskLevel` types so `aura-skills` can stay LLM-free.
- **agent** — Assembly layer: Actor, AgentLoop, ToolExecutor, ObservabilityRecorder, cost management (CostTracker, CostGuard), plus all domain managers (SessionManager, MemoryManager, TraceCollector, JobManager, SecretVault, SecurityGateway, CronScheduler). Bridges cron domain types and storage row types.
- **bootstrap** — Binary entry point (`src/main.rs`) and `boot` submodule. Loads `AuraConfig`, translates each section into domain types, and wires the Arc graph that `agent` consumes. Unit-tested mappings live in `boot`; Arc lifetime management stays in `main.rs`.
- **cli** — Operator-facing command layer (`aura-cli`). One `clap` tree drives both argv-mode commands (`aura config show`) and in-conversation slash commands (`/config show`). Read-only and mutating commands share a single dispatcher; slash input that resolves to a CLI command never enters the agent's context. User-invocable skills are the one sanctioned exception: `/<skill>` is forwarded to the agent as a normal chat message so `SkillRegistry::select` can narrow on the exact-match branch.

## Cross-Cutting Guides

- [testing.md](../testing.md) — Test pyramid (unit / crate-level / cross-crate), `test-support` gating, fixture inventory, and the six conventions every new test should follow.

## Dependency Overview

```
model (owns Session/User/ChannelType/SessionState + memory/message types; no internal deps)
  ├── channels ──► model
  ├── llm ──► model
  ├── context ──► model
  ├── security ──► model, channels
  ├── hook ──► channels
  ├── trace ──► model, context, job
  ├── tools ──► model
  ├── cron ──► model
  ├── skills ──► model
  ├── skills-assessor ──► skills, storage, llm, model
  └── job (no internal deps)
  └── workspace (no internal deps)
  └── config (no internal deps; external only)

storage   ──► model, trace, security, job (defines all Store traits; sole backend: libsql; CronStore uses opaque row types)
session   ──► model, storage (owns SessionManager; consumes SessionStore from storage)
agent     ──► model, llm, tools, workspace, context, session, trace, job, cron, security, storage, hook, channels, config
bootstrap ──► config + all domain crates it assembles (entry point only)
```

## Key Constraints

- Each module defines its own error type; no shared error enum
- Store traits defined in `storage`; domain types in their own crates; business logic in `agent`; `model` contains shared content primitives and memory domain types
- Logs, Trace, and Job must not record sensitive plaintext — only placeholders or sanitized summaries
- Tool/skill extensions must carry source, version, hash, trust level, and capability declarations
- The Job state machine is fixed: `Pending → InProgress → Completed → Submitted → Accepted` (with `Failed` and `Stuck` branches)
- Multimedia passed by reference — no raw binary in sessions, snapshots, or Trace
- Hot reload, tool updates, identity changes, and config changes must leave provenance records in Trace
- Cron and background execution must all enter Job and Trace
