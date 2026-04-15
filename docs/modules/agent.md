# agent - Assembly Layer and Execution Engine

## Overview

The `agent` crate is Aura's top-level assembly layer, connecting all other modules into an executable engine.

Core responsibilities:

- **Message dispatch**: Actor model, one Actor per session for isolation
- **Agent main loop**: LLM calls, tool/skill execution, reply generation
- **Business logic managers**: `SessionManager`, `MemoryManager`, `TraceCollector`, `JobManager`, `SecretVault`, `SecurityGateway` — all domain managers live here
- **Long-running execution**: cron scheduling, background notifications
- **Unified observability**: wrapping Job, Trace, and Cost through `ObservabilityRecorder`
- **Cost management**: `CostTracker` for recording, `CostGuard` for spending limits (in `agent::cost`)
- **Runtime logic**: error recovery, timeout control, rollback

It does not own low-level storage or backend implementation — it consumes Store traits from `storage` through dependency injection. Domain types come from their respective crates (`session`, `model`, `trace`, `security`, `job`, `cron`). Each manager defines its own error type for business-level failures (e.g. `MemoryManager` defines errors for embedding and dedup failures).

## Design Decisions

### Actor isolation model

One Actor per session: natural serialization within a session (no context races), natural concurrency across sessions. All control messages (rollback, timeout, cron) route to the same actor.

### Main execution path (AgentLoop)

1. Build system prompt, Soul, identity injection from `workspace`
2. Recall long-term memory
3. Append current user message to Context
4. Skill selection (derived from `SkillRegistry::select`):
   - An exact `/<cmd>` message returns just that one skill; any other message returns the full registered set. No scoring or mention-scanning happens — narrowing is handled upstream by the slash-equality check, not by heuristic ranking.
   - Every returned candidate runs through `SkillAssessor` (`aura-skills-assessor`). `Dangerous` verdicts drop the skill *and* emit `AgentOutput::Notice { level: Error }`; `Suspicious` verdicts keep the skill and emit `Notice { level: Warn }`; `Safe` verdicts pass silently.
   - Admitted skills have their `prompt_template` rendered via `aura_skills::render::render_skill_block` and injected as a system message, their names recorded on `session.state.active_skills`, and their `allowed_tools` unioned into this turn's tool ceiling.
5. Loop: `maybe_compress()` → build `ChatRequest` → call `LlmClient` → parse response → dispatch tool execution
6. Emit `OutgoingMessage` and persist Job, Trace, and Cost state

### ObservabilityRecorder lock strategy

`ObservabilityRecorder` exposes short-lived `begin/succeed/fail`. `AgentLoop` and `ToolExecutor` must never hold locks while waiting for LLM calls or tool execution.

### ToolExecutor responsibility

ToolExecutor: lookup tool → read declared secrets → construct `ToolContext` → create child Job/Trace nodes → execute → write results. It does **not** decide whether a tool should be called — that's `AgentLoop`.

### Long-running model

Cron jobs flow through the Actor model and observability chain: `CronScheduler` → `Router` → `AgentSupervisor` → `AgentMessage::CronTrigger` → `AgentLoop`. All create Job and Trace records. Background results are delivered asynchronously without polluting foreground conversation. Cron jobs are bound to `user_id + channel` (not `session_id`) so they survive session expiration; sessions are resolved dynamically at trigger time.

### Rollback mechanism

`Rollback` message → read snapshot from `TraceCollector` → `fork_from(target_node)` → restore session messages and context state from snapshot.

### Startup recovery

On system startup, before accepting any messages, `JobManager::recover_interrupted()` scans all non-terminal jobs left over from a prior run. `InProgress` jobs are moved to `Stuck` (their executing context was lost); other non-terminal states are left unchanged. This is called once in `main()` after constructing the `JobManager`.

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
| `model` | Provides memory domain types (`MemoryEntry`, `MemoryCategory`) used by `agent::memory::MemoryManager` |
| `workspace` | Identity files for system prompt |
| `cron` | Provides `CronJob`, `CronExecution` domain types; `CronScheduler` in agent manages lifecycle, converts between domain and storage row types |
| `context` | Conversation window and compression |
| `job` | Provides domain types (`Job`, `JobStatus`, `OperationKind`) used by `agent::job::JobManager`; startup recovery via `recover_interrupted()` |
| `trace` | Provides domain types and tree/fork/snapshot utilities used by `agent::trace::TraceCollector` |
| `session` | Provides domain types (`Session`, `User`, `ChannelType`) used by `agent::session::SessionManager` |
| `security` | Provides crypto primitives (`EncryptionKey`, `LeakDetector`) used by `agent::security::{SecretVault, SecurityGateway}` |
| `channels` | `ChannelRegistry` and adapters (e.g. `TuiAdapter`); Router owns the registry for dispatch |
| `storage` | Provides all Store traits and libsql implementations; injected into managers |
| `hook` | `AgentActor` triggers `PreMessage` and `PreResponse` hooks at lifecycle points |
