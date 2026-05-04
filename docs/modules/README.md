# Aura Module Documentation Index

Each document covers: module responsibilities, design decisions, key constraints, and collaboration with other modules.

## Reading Order

Bottom-up along the dependency graph:

1. [model.md](model.md) → [config.md](config.md) → [session.md](session.md) → [channels.md](channels.md)
2. [job.md](job.md) → [cron.md](cron.md) → [skills.md](skills.md)
3. [llm.md](llm.md) → [security.md](security.md)
4. [tools.md](tools.md) → [workspace.md](workspace.md) → [context.md](context.md)
5. [trace.md](trace.md) → [hook.md](hook.md)
6. [storage.md](storage.md) → [pairing.md](pairing.md) → [agent.md](agent.md) → [bootstrap.md](bootstrap.md) → [cli.md](cli.md) → [gateway.md](gateway.md) → [tui.md](tui.md)

## Module Groups

### Foundational Types Layer

- **model** — Shared content primitives (ChatMessage, ContentBlock, Role, BlobRef, MessageMetadata), memory domain types (MemoryEntry, MemoryCategory), and governance types (TrustLevel, ArtifactSource, ExtensionManifest). No business traits.
- **config** — Root `AuraConfig` with JSON loading and `validate()`. Sections (llm, agent, session, channels, security, tools, trace, cost, workspace). Uses mirror structs to stay decoupled from domain crates.

### Ingress and Security Boundary Layer

- **session** — `SessionError` and `SessionManager` (lifecycle logic). Session domain types (`User`, `ChannelType`, `Session`, `SessionState`, `TriggerSource`, `SystemReason`, `Lineage`) live in `model`; the `SessionStore` trait lives in `storage`. A `Session` is the top of one trace tree (1 trace = 1 session); fork and subagent spawn create new sessions linked through `Lineage`. `aura-session` depends on both `model` and `storage`.
- **channels** — Channel adapter trait, shared message types (Message, IncomingMessage, OutgoingMessage), slash/dashboard trait definitions (`SlashHandler`, `DashboardProvider`, `ViewKind`), and `ChannelRegistry`. No built-in adapters — the terminal UI now lives in its own `aura-tui` crate (see [`tui.md`](./tui.md)).
- **security** — Cryptographic primitives (EncryptionKey, encrypt/decrypt), leak detection (LeakDetector), error types.

### Capability and Governance Layer

- **llm** — LLM provider wrapping and response parsing. Subscription/OAuth flavoured providers documented in [`llm-openai-subscription.md`](llm-openai-subscription.md).
- **tools** — Tool abstraction, registration, capability declarations, runtime routing. The `mcp` submodule ships an MCP client (config in `<workspace>/.mcp.json`, OAuth via rmcp) that surfaces every server's tools to the agent loop as `<server>/<tool>`; the `McpReconciler` keeps the registry in sync without a gateway restart.
- **[sandbox](sandbox.md)** — OS-native per-invocation isolation for tools declaring `ToolCapability::ExecCommand`. `bwrap` on Linux, `sandbox-exec` on macOS, `docker` as a cross-platform fallback when the native backend is unavailable; the `ToolExecutor` injects a `SandboxAdapter` into `ToolContext.sandbox` so `BashTool` (and any future ExecCommand tools) routes its child process through the platform's isolation primitive. Filesystem-scoped to the workspace; network gated all-or-nothing on the manifest's `Http` capability.
- **skills** — Declarative skill definitions, selection, trust tiers, hot reload.
- **[skills-assessor](skills-assessor.md)** — LLM-backed risk classifier for skills. Hashes the skill directory, caches verdicts (`Safe`/`Suspicious`/`Dangerous`) in `SkillRiskStore`, tiers large skills (primary-scope synchronous + full-scope background worker with restart-safe job recovery), and gates skill injection in `AgentLoop` so only `Dangerous` blocks. Kept separate from `skills` so selection stays deterministic and offline-capable.
- **workspace** — Identity files and long-running configuration.
- **cron** — Cron job domain types (`CronJob`, `CronExecution`, `CronStatus`, `CronRunMode`, `CronError`). Standard cron syntax.
- **context** — Context appending and compression.

### Runtime and Observability Layer

- **trace** — Step / Span / SpanEvent domain types (`Step`, `StepKind`, `Span`, `SpanKind`, `SpanEvent`, `SpanEventKind`, `LlmToolCallRecord`, `ToolCallOrigin`) and the half-open-span recovery utility. Closed strong-typed enums; OTel-aligned naming.
- **job** — Job domain types (`Job`, `JobStatus`, `JobKind`, `JobInput`, `JobOutput`, `CancelReason`, `JobTransition`, `DriftRecord`) and state machine. `Cancelled` and `Failed` are independent terminal states.
- **hook** — Lifecycle extension points.

### Infrastructure and Assembly Layer

- **storage** — Defines all Store traits (`SessionStore`, `MemoryStore`, `TraceStore`, `SecretStore`, `JobStore`, `CostStore`, `CronStore`, `SkillRiskStore`, `ChannelSessionStore`, `ChannelBotStore`, `ChannelPairingStore`); implements all via libsql (single backend). `CronStore` uses opaque row types (`CronJobRow`, `CronExecutionRow`) — no dependency on `cron` domain crate. `SkillRiskStore` defines its own `RiskVerdict` / `RiskLevel` types so `aura-skills` can stay LLM-free. `ChannelPairingStore` defines `ChannelPairingRow` / `PairingStatus` so `aura-pairing` can stay a business-logic crate.
- **[pairing](pairing.md)** — Per-user pairing gate for sidecar-routed inbound messages. `PairingService` checks the `(channel_type, bot_id, user_id)` triple, mints 6-char codes for unknown senders, and refuses with a `Frame::Notice` until `aura pair approve <code>` flips the row to `approved`. Store trait + row lives in `storage`; `aura-pairing` is the service + code generator.
- **agent** — Assembly layer: Actor, AgentLoop, ToolExecutor, observability facades (`JobLifecycle` for the job state machine and lifecycle hooks, `SpanRecorder` for Step/Span/SpanEvent writes), cost management (`CostTracker` as a `TraceEventStream` subscriber, `CostGuard`), plus all domain managers (SessionManager, MemoryManager, SecretVault, SecurityGateway, CronScheduler). Bridges cron domain types and storage row types.
- **bootstrap** — Binary entry point (`src/main.rs`) and `boot` submodule. Loads `AuraConfig`, translates each section into domain types, and wires the Arc graph that `agent` consumes. Unit-tested mappings live in `boot`; Arc lifetime management stays in `main.rs`.
- **cli** — Operator-facing command layer (`aura-cli`). One `clap` tree drives both argv-mode commands (`aura config show`) and in-conversation slash commands (`/config show`). Read-only and mutating commands share a single dispatcher; slash input that resolves to a CLI command never enters the agent's context. User-invocable skills are the one sanctioned exception: `/<skill>` is forwarded to the agent as a normal chat message so `SkillRegistry::select` can narrow on the exact-match branch.
- **[tui](tui.md)** — Interactive terminal UI (`aura-tui`). Ratatui + Crossterm frontend driven by a WS+MessagePack `WsTransport` client of `aura-gateway`; no local manager graph, no workspace singleton. Hosts `TuiAdapter`, `TuiSlashHandler`, and `TuiDashboardProvider`. Input-history persistence is delivered over the same WS via `Frame::HistorySnapshot` / `Frame::HistoryAppend` — the TUI never opens the vault itself. Depends on `aura-channels` for shared trait definitions only.
- **[gateway](gateway.md)** — Headless HTTP backend (`aura-gateway`). One axum server is both a `ChannelType::Http` adapter (chat flows through the normal Router path) and an admin REST/SSE API mirroring the CLI families. Auth is a dynamic per-install token stored in `SecretVault`; platform service units live behind `linux` / `macos` Cargo features (one knob per OS; reuse these for any future platform-specific gateway code). Driven by the `aura gateway …` command tree.

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
pairing   ──► model, storage (owns PairingService + code generator; consumes ChannelPairingStore from storage)
sandbox   ──► (no internal deps; OS sandbox runner consumed by agent)
agent     ──► model, llm, tools, workspace, context, session, trace, job, cron, security, sandbox, storage, hook, channels, config
gateway   ──► agent, channels, config, cron, job, llm, model, pairing, security, session, skills, storage, tools, trace, workspace
tui       ──► channels, model, tools (trait defs + shared types; talks to gateway over HTTP+SSE)
bootstrap ──► config + all domain crates it assembles (entry point only)
```

## Key Constraints

- Each module defines its own error type; no shared error enum
- Store traits defined in `storage`; domain types in their own crates; business logic in `agent`; `model` contains shared content primitives and memory domain types
- Logs, Trace, and Job must not record sensitive plaintext — only placeholders or sanitized summaries
- Tool/skill extensions must carry source, version, hash, trust level, and capability declarations
- The Job state machine is fixed: `Pending → InProgress → Completed` (with `Stuck`, `Failed`, and `Cancelled` branches). `Cancelled` carries a `reason` and `partial_artifacts: Vec<SpanId>` for resume context
- Multimedia passed by reference — no raw binary in sessions or Trace
- Hot reload, tool updates, identity changes, and config changes must leave provenance records in Trace
- Cron and background execution must all enter Job and Trace
