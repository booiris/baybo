# agent - Assembly Layer and Execution Engine

## Overview

The `agent` crate is Aura's top-level assembly layer, connecting all other modules into an executable engine.

Core responsibilities:

- **Message dispatch**: Actor model, one Actor per session for isolation
- **Agent main loop**: LLM calls, tool/skill execution, reply generation
- **Business logic managers**: `SessionManager`, `MemoryManager`, `TraceCollector`, `JobManager`, `SecretVault`, `SecurityGateway` — all domain managers live here
- **Long-running execution**: heartbeat, routine, cron, background notifications
- **Unified observability**: wrapping Job, Trace, and Cost through `ObservabilityRecorder`
- **Cost management**: `CostTracker` for recording, `CostGuard` for spending limits (in `agent::cost`)
- **Runtime logic**: error recovery, timeout control, rollback

It does not own low-level storage or backend implementation — it consumes Store traits from `storage` through dependency injection. Domain types and errors come from their respective crates (`session`, `memory`, `trace`, `security`, `job`).

## Design Decisions

### Actor isolation model

One Actor per session: natural serialization within a session (no context races), natural concurrency across sessions. All control messages (rollback, timeout, cron, heartbeat, routine) route to the same actor.

### Main execution path (AgentLoop)

1. Create top-level Job and Trace span
2. Build system prompt, Soul, identity injection from `workspace`
3. Recall long-term memory
4. Append current user message to Context
5. Loop: `maybe_compress()` → build `ChatRequest` → call `LlmClient` → parse response → dispatch tool/skill execution
6. Produce final `OutgoingMessage`
7. Persist final Job, Trace, and Cost state

### ObservabilityRecorder lock strategy

`ObservabilityRecorder` exposes short-lived `begin/succeed/fail`. `AgentLoop` and `ToolExecutor` must never hold locks while waiting for LLM calls or tool execution.

### ToolExecutor responsibility

ToolExecutor: lookup tool → read declared secrets → determine sandbox/network policy → construct `ToolContext` → create child Job/Trace nodes → execute → write results. It does **not** decide whether a tool should be called — that's `AgentLoop`.

### Long-running model

Heartbeat, routine, and cron all flow through the Actor model and observability chain: `HeartbeatRunner/RoutineScheduler` → `AgentSupervisor` → `AgentMessage` → `AgentLoop`. All create Job and Trace records. Background results are delivered asynchronously without polluting foreground conversation.

### Rollback mechanism

`Rollback` message → read snapshot from `TraceCollector` → `fork_from(target_node)` → restore session messages and context state from snapshot.

### Router's upstream responsibilities

Before a message enters an actor, Router completes: session identification/creation, user-level rate limiting, quota check via `CostGuard`, select/create target `AgentActor`.

## Constraints

- Top-level assembly module — depends on all business crates
- Keep `AgentActor` thin; prevent it from becoming a God Object
- Set `max_iterations` on the main `run()` loop
- Background notification targets must be explicitly configured

## Collaboration

| Module | Role |
|--------|------|
| `llm` | `AgentLoop` initiates model calls |
| `tools` | `ToolExecutor` executes tools |
| `skills` | `AgentLoop` parses and executes skills |
| `memory` | Provides domain types (`MemoryEntry`, `MemoryCategory`) used by `agent::memory::MemoryManager` |
| `workspace` | Identity files, heartbeat config, routine definitions |
| `context` | Conversation window and compression |
| `job` | Provides domain types (`Job`, `JobStatus`, `OperationKind`) used by `agent::job::JobManager` |
| `trace` | Provides domain types and tree/fork/snapshot utilities used by `agent::trace::TraceCollector` |
| `session` | Provides domain types (`Session`, `User`, `ChannelType`) used by `agent::session::SessionManager` |
| `security` | Provides crypto primitives (`EncryptionKey`, `LeakDetector`) used by `agent::security::{SecretVault, SecurityGateway}` |
| `storage` | Provides all Store traits and libsql implementations; injected into managers |
| `sandbox` | WASM or container isolated execution |
| `hook` | `AgentActor` triggers hooks at lifecycle points |
