# agent - Assembly Layer and Execution Engine

## Overview

The `agent` crate is Aura's top-level assembly layer, connecting all other modules into an executable engine.

Core responsibilities:

- **Message dispatch**: Actor model, one Actor per session for isolation
- **Agent main loop**: LLM calls, tool/skill execution, reply generation
- **Business logic managers**: `SessionManager` (in `aura-session`), `JobLifecycle` (in `aura-job`), `MemoryManager`, `SpanRecorder`, `SecretVault`, `SecurityGateway` — most domain managers live in their respective domain crates now; `agent` assembles them. `SecurityGateway` stays here because it is a cross-cutting interception facade tied to the execution path
- **Long-running execution**: cron scheduling, background notifications
- **Unified observability**: `SpanRecorder` (Step / Span / SpanEvent) and `JobLifecycle` (Job state machine, in `aura-job`)
- **Cost management**: `CostManager` records LLM-call cost and gates spend; `CostGuardError`, `CostMetrics`, `SpendingLimits` round out `agent::cost`
- **Runtime logic**: error recovery, timeout control

It does not own low-level storage or backend implementation — it consumes Store traits from `aura-job` (for `JobStore`) and `aura-storage` (for the rest) through dependency injection. Domain types come from their respective crates (`session`, `model`, `trace`, `security`, `job`, `cron`). Each manager defines its own error type for business-level failures (e.g. `MemoryManager` defines errors for embedding and dedup failures).

## Design Decisions

### Actor isolation model

One Actor per session: natural serialization within a session (no context races), natural concurrency across sessions. All control messages (timeout, cron) route to the same actor.

### Main execution path (AgentLoop)

1. Build system prompt, Soul, identity injection from `workspace` — `Soul` reads the three identity files (`profile/{SOUL,USER,IDENTITY}.md`) once here and bakes them into a single `system_prompt` `String`. Mid-session writes (e.g. via `Edit` against a path under `profile/`) are **not** picked up by this session; see [`docs/todo/profile-hot-reload.md`](../todo/profile-hot-reload.md).
2. Recall long-term memory
3. Append current user message to Context
4. Skill selection (`AgentLoop::invocable_skills`): `SkillRegistry::all_summaries_sorted()` filtered by `agent_invocable && trust_level != Untrusted`. Risk assessment fires later, inside `SkillTool` at invocation time (see `crates/skills/src/tools.rs`), not during selection. Selected skill names are recorded on `session.state.active_skills`; when the set has changed since last turn the full list is rebroadcast as a `Role::User` skill reminder before the user message.
5. Loop: `maybe_compress()` → build `ChatRequest` → call `LlmClient` → parse response → dispatch tool execution
6. Emit `OutgoingMessage` and persist Job, Trace, and Cost state

### SpanRecorder lock strategy

`SpanRecorder` exposes short-lived `begin/succeed/fail`. `AgentLoop` and `ToolExecutor` must never hold locks while waiting for LLM calls or tool execution.

### ToolExecutor responsibility

ToolExecutor: lookup tool → validate trust/capability → consult approval gate → construct `ToolContext` → create child Job/Trace nodes → reveal placeholders in args → execute → sanitize output → write results. It does **not** decide whether a tool should be called — that's `AgentLoop`.

`ToolExecutor` holds an `Arc<SecurityGateway>`. Tool invocation is the one legitimate plaintext boundary for arguments: the pre-reveal `params` is what flows into `SpanInput::ToolExecution` and the approval preview (placeholder form), while a cloned `params_revealed` — with `reveal_in_value` applied — is what's passed to `tool_registry.execute`. After execution the returned `ToolOutput` is run through `sanitize_tool_output` so any tool-echoed secret is re-minted and vaulted before it enters the trace, the next LLM call, or memory. Errors are passed through `sanitize_error` before `recorder.fail`.

### LLM-response defensive scrubbing

`AgentLoop` holds an `Arc<SecurityGateway>`. In `call_llm`, every `LlmResponse` — including `content`, `content_blocks` text, `thinking`, and `tool_calls[*].arguments` — is run through `SecurityGateway::sanitize_llm_response` *before* the response is recorded to the trace, appended to `session.messages`, or passed to the memory manager. This prevents LLM-fabricated secret-shaped strings from leaking into any downstream sink.

### Tool-result formatting into LLM context

After `ToolExecutor::execute` returns, `AgentLoop` renders the result into a text blob (`ToolOutput::Text` → raw; `ToolOutput::Json` → serialized; `ToolOutput::Error` and errors → a prefixed error line), then pipes it through the security gateway: `cap_tool_output` first (so the truncation notice lands inside the envelope) and `wrap_tool_output_for_llm(&tool_name, ...)` second. The wrapped string is what populates `ContentBlock::ToolResult { content }`. This gives the LLM a clear, forgery-resistant boundary around untrusted tool output and limits any single tool call's context impact to `MAX_TOOL_OUTPUT_BYTES` bytes.

### Streaming delta reveal

`AgentLoop::chat_streaming` is the only path that emits plaintext secrets. Raw chunks accumulate into a `pending` buffer; `safe_flush_boundary` returns the largest prefix that cannot contain a partial placeholder (last unmatched `[{`, or a lone trailing `[`). Buffer size is capped at `STREAM_BUFFER_HIGH_WATER = 128` bytes to force flushes under pathological input. The flushable prefix is scanned/minted/vaulted once; the placeholder form is appended to the `LlmResponse.content` accumulator that the caller returns (so trace and memory see placeholders), while `reveal_in_text` is applied to the copy sent to `delta_tx` for user-facing display.

### Approval gate wiring

`ToolExecutor` holds an `Arc<ApprovalGateMap>` shared with `ChannelRegistry`. The map is populated automatically when channels register — `ChannelRegistry::register` reads `Channel::approval_gate()` and inserts the returned `Arc<dyn ApprovalGate>` keyed by the channel's `ChannelType`, and evicts it on `unregister`. For every call:

1. Resolve the gate for the session's channel via `gate_map.get(user.channel)`.
2. Compute `ResourceAccess` list via the tool's `accessed_resources(params)`.
3. Filter out entries covered by the snapshot of `SessionState::approved_resources` passed in from `AgentLoop`.
4. If any remain, call `gate.request(...)` with the uncovered set and a truncated params preview. On `Deny` the call short-circuits to `ToolError::Denied` (recorded on the trace before return).
5. On `ApproveAlways`, the executor de-dupes and pushes the newly-approved accesses directly into the shared `Mutex<Vec<ApprovedResource>>` passed by `AgentLoop`. After all tool calls complete, `AgentLoop` flushes the contents back into `session.state.approved_resources`, which persists through session save/restore because the types live in `aura-model`.

Parallel tool calls within a turn each go through the gate independently; the gate implementation is responsible for its own serialization (TUI queues and shows one inline prompt at a time).

### Long-running model

Cron jobs flow through the Actor model and observability chain: `CronScheduler` → `Router` → `AgentSupervisor` → `AgentMessage::CronTrigger` → `AgentLoop`. All create Job and Trace records. Background results are delivered asynchronously without polluting foreground conversation. Cron jobs are bound to `user_id + channel` (not `session_id`) so they survive session expiration; sessions are resolved dynamically at trigger time.

`AgentMessage::CronTrigger { job_id, prompt }` carries the cron job id and the prompt string directly. `AgentActor` dispatches `prompt` through `dispatch_prompt` with `JobInput::Cron`, which runs the normal `AgentLoop` path; the LLM decides what tools (if any) to invoke.

### LLM-invocable cron tools

`aura_cron::agent_tools` returns `CronCreateTool`, `CronDeleteTool`, and `CronListTool` — `Tool` trait implementations that let the LLM schedule/cancel/inspect cron jobs mid-conversation. They live in `aura-cron` (not `aura-tools`) because they each hold `Arc<CronScheduler>`, and `aura-tools` cannot pull in `aura-cron` without creating a circular dependency. `src/main.rs` registers them after the scheduler is constructed, via `Arc::get_mut(&mut tool_registry)` while no other clones exist yet.

### Startup recovery

Not implemented yet — see `docs/modules/job.md` "Restart recovery" and `docs/modules/trace.md` "Restart recovery". After a crash, in-flight jobs and half-open spans stay in their last-persisted state until an operator cancels them via the admin API.

### Router's upstream responsibilities

Before a message enters an actor, Router completes: session identification/creation, user-level rate limiting, quota check via `CostManager::check`, select/create target `AgentActor`.

### Actor-side slash commands

`AgentActor::handle_user_input` inspects the leading text block of every inbound `IncomingMessage` for control slash commands before routing into `run_agent_loop`. Today the only one is `/compact`, which calls `AgentLoop::compact_now` — the method mints a job (matching the session's trigger kind), drives `ContextManager::force_compress` via the same `CompressionRunner` the iteration-top path uses, and returns the before/after-token confirmation text. The actor wraps that text as `AgentOutput::Notice { level: Info, ... }` rather than `Message` so the response renders out-of-band and stays out of the assistant transcript (the user typed a control command, not a question). Trailing arguments are ignored; matching is case-insensitive on the command token. Sidecar channels learn the command via the gateway slash manifest (`crates/gateway/src/channel/slash.rs::manifest`), but the gateway dispatcher passes it through unchanged — only `/new` needs server-side state.

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
| `model` | Provides memory domain types (`MemoryEntry`, `MemoryCategory`) used by `agent::memory::MemoryManager`; session domain types (`Session`, `User`, `ChannelType`) used by `agent::session::SessionManager` |
| `workspace` | Identity files for system prompt |
| `cron` | Owns `CronJob`, `CronExecution`, and `CronScheduler`; agent re-exports `CronScheduler` / `CronTriggerEvent` for assembly-layer wiring |
| `context` | Conversation window and compression |
| `job` | Owns `Job`, `JobStatus`, `JobKind`, `JobStore` trait, and `JobLifecycle` (persistence orchestrator + cancellation registry + terminal-event bus). Agent constructs and shares one `JobLifecycle` across the loop, router, supervisor, and subagent wait routine |
| `trace` | Provides domain types and tree/fork utilities used by `agent::trace::SpanRecorder` |
| `session` | Provides `SessionManager` and its error type (domain types live in `aura-model`) |
| `security` | Provides crypto primitives, `SecretVault`, `SecretValue`, `LeakDetector`, `PlaceholderMinter`, `InjectionDetector`; `agent::security::SecurityGateway` composes them |
| `channels` | `Channel` handles + `ChannelRegistry`; Router owns the registry for dispatch by `ChannelType` |
| `storage` | Provides remaining Store traits and the libsql implementations of all stores; `JobStore` trait moved to `aura-job` |
