# agent - Assembly Layer and Execution Engine

## Overview

The `agent` crate is Baybo's top-level assembly layer, connecting all other modules into an executable engine.

Core responsibilities:

- **Message dispatch**: Actor model, one Actor per session for isolation
- **Agent main loop**: LLM calls, tool/skill execution, reply generation
- **Business logic managers**: `SessionManager` (in `baybo-session`), `JobLifecycle` (in `baybo-job`), `SpanRecorder` (in `baybo-trace`), the `Memory` trait (in `baybo-memory`), `CostManager` (in `baybo-cost`), `SecretVault` (in `baybo-security`), `SecurityGateway` — all domain managers live in their respective domain crates now; `agent` assembles them. `SecurityGateway` stays here because it is a cross-cutting interception facade tied to the execution path
- **Long-running execution**: cron scheduling, background notifications
- **Unified observability**: `SpanRecorder` (in `baybo-trace`, Step / Span / SpanEvent) and `JobLifecycle` (in `baybo-job`, Job state machine)
- **Cost management**: `CostManager` (in `baybo-cost`) records LLM-call cost and gates spend; agent constructs it and threads it through the loop
- **Runtime logic**: error recovery, timeout control

It does not own low-level storage or backend implementation — it consumes every `*Store` trait from the `baybo-store` ports crate through dependency injection, and the libsql impls (`baybo-storage`) are wired in at assembly time. Domain managers and rich types come from their respective crates (`session`, `model`, `trace`, `security`, `job`, `cron`); the `JobStore` / `TraceStore` it calls trade in row DTOs that `baybo-job` / `baybo-trace` convert to and from. Each manager defines its own error type for business-level failures (e.g. `JobLifecycle` defines errors for invalid state transitions).

## Source Layout

`src/` is split along two axes — per-turn execution and per-session actor orchestration — plus the cross-cutting policy / process-level infrastructure that lives outside either bucket:

```
agent/src/
├── lib.rs
├── security.rs               # SecurityGateway (cross-cutting interception facade)
├── service.rs                # ShutdownSignal, TaskTracker (process-level)
├── runtime/                  # per-turn execution core
│   ├── agent_loop.rs         # AgentLoop, AgentLoopConfig
│   ├── tool_executor.rs      # ToolExecutor + approval gate; wires virtual-file providers into ToolContext
│   ├── virtual_read.rs       # SessionTranscriptReader: VirtualReadResolver serving the transcript (ReadTool consults it)
│   ├── compression.rs        # inline + background compression wiring
│   ├── soul.rs               # system-prompt + identity assembly
│   ├── billed_chat.rs        # cost-aware LLM call wrapper
│   ├── error_recovery.rs     # retry / degrade policy
│   ├── sandbox.rs            # SandboxAdapter glue for tool exec
│   ├── scope.rs              # with_job / with_step / with_span guards
│   └── llm_pool.rs           # per-provider LlmClient pool
└── actor/                    # per-session actor + orchestration
    ├── mod.rs                # AgentActor + AgentMessage
    ├── runner.rs             # tokio task boundary + actor panic recovery
    ├── supervisor.rs         # AgentSupervisor + idle reaper
    ├── subagent.rs           # subagent wait routine
    ├── router/               # ingress dispatch (cron / user / output / system_spawn)
    └── state/                # DurableActorState + VolatileResources
```

For backwards compatibility, `lib.rs` re-exports the submodules at the crate root (`baybo_agent::agent_loop`, `baybo_agent::supervisor`, etc.), so consumers don't see the directory split unless they want to.

## Design Decisions

### Actor isolation model

One Actor per session: natural serialization within a session (no context races), natural concurrency across sessions. All control messages (timeout, cron) route to the same actor.

### Main execution path (AgentLoop)

1. System-prompt assembly lives in [`baybo_context::prompts::soul`](context.md) (`assemble_from_workspace` reads `profile/{SOUL,USER,IDENTITY}.md` and frames them with TOP/TAIL hints). `ContextManager` owns the whole system-prompt lifecycle: `ensure_seeded()` resolves the prompt (`resolve_system_prompt` — a subagent profile looked up by name in the registry, else the workspace soul assembled fresh, else a fallback), seeds the leading `System` row, and appends the skill reminder. `AgentLoop` reaches it only through the thin `ensure_system_prompt_seeded` seam, which delegates to `ensure_seeded()`. The reseed-after-compaction re-resolves from the same source, so a mid-session profile write is picked up on the next compaction (which re-reads the workspace), not mid-turn.
2. Append current user message to Context
3. Skill selection (`ContextManager::invocable_skill_summaries()`): `SkillRegistry::all_summaries_sorted()` filtered by `agent_invocable && trust_level != Untrusted`. The same set backs the seed-time skill reminder and the per-turn `/command` candidate list, so the advertised and slash-invocable sets can't drift. Risk assessment fires later, inside `SkillTool` at invocation time (see `crates/skills/src/tools.rs`), not during selection — except an explicit user `/command`, which `ContextManager::expand_slash_command` treats as authorized and injects directly. The skill reminder is seeded once by `ensure_seeded` (and re-inserted after each compaction), not rebroadcast per turn.
4. Loop: `maybe_compress()` → build `ChatRequest` → call `LlmClient` → parse response → dispatch tool execution
5. Emit `OutgoingMessage` and persist Job, Trace, and Cost state

### SpanRecorder lock strategy

`SpanRecorder` exposes short-lived `begin/succeed/fail`. `AgentLoop` and `ToolExecutor` must never hold locks while waiting for LLM calls or tool execution.

### ToolExecutor responsibility

ToolExecutor: lookup tool → validate trust/capability → consult approval gate → construct `ToolContext` → create child Job/Trace nodes → reveal placeholders in args → execute → sanitize output → write results. It does **not** decide whether a tool should be called — that's `AgentLoop`.

`ToolExecutor` holds an `Arc<SecurityGateway>`. Tool invocation is the one legitimate plaintext boundary for arguments: the pre-reveal `params` is what flows into `SpanInput::ToolExecution` and the approval preview (placeholder form), while a cloned `params_revealed` — with `reveal_in_value` applied — is what's passed to `tool_registry.execute`. After execution the returned `ToolOutput` is run through `sanitize_tool_output` so any tool-echoed secret is re-minted and vaulted before it enters the trace, the next LLM call, or memory. Errors are passed through `sanitize_error` before `recorder.fail`.

### LLM-response defensive scrubbing

`AgentLoop` holds an `Arc<SecurityGateway>`. In `call_llm`, every `LlmResponse` — including `content`, `content_blocks` text, `thinking`, and `tool_calls[*].arguments` — is run through `SecurityGateway::sanitize_llm_response` *before* the response is recorded to the trace or appended to `session.messages`. This prevents LLM-fabricated secret-shaped strings from leaking into any downstream sink.

### Tool-result formatting into LLM context

After `ToolExecutor::execute` returns, `AgentLoop` renders the result into a text blob (`ToolOutput::Text` → raw; `ToolOutput::Json` → serialized; `ToolOutput::Error` and errors → a prefixed error line), then bridges the **detect/format split**: `context_manager.cap_tool_output` first (caps to `MAX_TOOL_OUTPUT_BYTES`, spilling oversize payloads under the workspace's tool-spills dir so the truncation notice lands inside the envelope), then `SecurityGateway::detect_injection` (the scan stays in `baybo-security`), then `baybo_context::prompts::tool_output::wrap_tool_output(&tool_name, &capped, &warning_rules)` (the `<tool_output>` envelope + breakout-escape + injection banner). The wrapped string populates `ContentBlock::ToolResult { content }`. The framing lives in `baybo-context`; only the scan stays in security, and the shared `</tool_output>` delimiter is `baybo_model::TOOL_OUTPUT_{OPEN,CLOSE}_PREFIX`.

### Streaming delta reveal

`AgentLoop::chat_streaming` is the only path that emits plaintext secrets. Raw chunks accumulate into a `pending` buffer; `safe_flush_boundary` returns the largest prefix that cannot contain a partial placeholder (last unmatched `[{`, or a lone trailing `[`). Buffer size is capped at `STREAM_BUFFER_HIGH_WATER = 128` bytes to force flushes under pathological input. The flushable prefix is scanned/minted/vaulted once; the placeholder form is appended to the `LlmResponse.content` accumulator that the caller returns (so trace and memory see placeholders), while `reveal_in_text` is applied to the copy sent to `delta_tx` for user-facing display.

### Approval gate wiring

`ToolExecutor` holds an `Arc<ApprovalGateMap>` shared with `ChannelRegistry`. The map is populated automatically when channels register — `ChannelRegistry::register` reads `Channel::approval_gate()` and inserts the returned `Arc<dyn ApprovalGate>` keyed by the channel's `ChannelType`, and evicts it on `unregister`. For every call:

1. Resolve the gate for the session's channel via `gate_map.get(user.channel)`.
2. Compute `ResourceAccess` list via the tool's `accessed_resources(params)`.
3. Filter out entries covered by the snapshot of `SessionState::approved_resources` passed in from `AgentLoop`.
4. If any remain, call `gate.request(...)` with the uncovered set and a truncated params preview. On `Deny` the call short-circuits to `ToolError::Denied` (recorded on the trace before return).
5. On `ApproveAlways`, the executor de-dupes and pushes the newly-approved accesses directly into the shared `Mutex<Vec<ApprovedResource>>` passed by `AgentLoop`. After all tool calls complete, `AgentLoop` flushes the contents back into `session.state.approved_resources`, which persists through session save/restore because the types live in `baybo-model`.

Parallel tool calls within a turn each go through the gate independently; the gate implementation is responsible for its own serialization (TUI queues and shows one inline prompt at a time).

### Long-running model

Cron jobs flow through the Actor model and observability chain: `CronScheduler` → `Router` → `AgentSupervisor` → `AgentMessage::CronTrigger` → `AgentLoop`. All create Job and Trace records. Background results are delivered asynchronously without polluting foreground conversation. Cron jobs are bound to `user_id + channel` (not `session_id`) so they survive session expiration; sessions are resolved dynamically at trigger time.

`AgentMessage::CronTrigger { job_id, prompt }` carries the cron job id and the prompt string directly. `AgentActor` dispatches `prompt` through `dispatch_cron_prompt` with `JobInput::Cron`, which appends the fire via `AgentLoop::append_cron_fire` (framed by `baybo_context::prompts::cron`, so it reads as a task, not a user message) and runs the normal `AgentLoop` path; the LLM decides what tools (if any) to invoke.

Background subagent results arrive as `AgentMessage::BackgroundJobFinished`, are buffered on `session.state.pending_background_results` (dedup by `handle_id`, cap 64 drop-oldest), and — once no higher-priority message is queued — drained into their own autonomous `SubagentNotification` agent-loop turn (same main path / system prompt + toolset, so the prompt cache holds; the model proactively reports to the user, and an empty reply is suppressed). The synthetic XML prompt for that turn (built by `baybo_context::prompts::subagent::build_notification_content`) is appended **in-memory only** (`AgentLoop::append_subagent_notification` → `ContextManager::append_in_memory`), never persisted to `session_messages`: it is rebuilt from the durable buffer on every retry, so persisting per-attempt would stack duplicate hidden rows under the infinite-backoff retry. On failure the turn rolls the in-memory context back to a pre-turn snapshot (after seeding the system prompt, so the rollback can't drop it) and re-buffers for retry.

The per-session mailbox is a **priority queue** (`mailbox::channel`): `UserInput`/trigger > `BackgroundJobFinished` > `ActorStop`. A rapid burst of `UserInput`s coalesces into one turn; a leading `/command` is a hard boundary.

**Mid-turn user interjection (steering).** A message the user sends *while a `UserChat` turn is running* is injected into that turn at the next tool boundary — drained from the mailbox at the top of each loop iteration after the first (before `compress_if_needed`, never mid-call), framed with a `<user_interjection>` steering envelope, and appended before the next LLM call. The loop reaches the mailbox through the `runtime::agent_loop::InterjectionSource` seam, which `AgentActor` implements over its `MailboxReceiver` (`MailboxInterjections`) using `MailboxReceiver::try_recv_if` to pop only the leading run of **non-slash** `UserInput`s — a queued slash command / `BackgroundJobFinished` / `ActorStop` stops the drain and is left for normal dispatch. Only `handle_merged_user_turn` (the non-slash user path) passes the source; cron / subagent-spawned / `/skill` / notification turns pass `None`. Each drained message is persisted as a faithful `MessageSource::UserInterjection` row (a clean user bubble — `from_user()` is true for it); the envelope is applied **wire-only** in `ContextManager::messages_for_llm` (`frame_interjections`, re-derived each call so it survives compaction). Non-preemptive: the in-flight tool/LLM call is never cancelled (`/stop` remains the only hard interrupt), and a message that never reaches a tool boundary (e.g. the turn ends with a `Final` response, or iteration 1 produced no tool calls) falls through to the next turn. See `docs/mid-turn-user-interjection.md`.

**`/stop`** is an out-of-band control command recognised in `Router::handle_incoming` (not the actor — a busy actor can't read its mailbox to preempt its own turn; the `@BotName` group-command suffix is stripped, mirroring the gateway slash parser). It cancels the session's in-flight turn + every in-flight subagent (foreground via job lineage `JobLifecycle::list_children`, background via the supervisor's `in_flight_background_subagents` registry). Background subagents are stopped by **cancelling the child's `CancellationToken`** — stored in the registry at dispatch, so this works even in the window *before* the child's job row exists (a job-store lookup would miss it and let it run on); the job is also cancelled `UserStopped` best-effort for audit when the row exists. Draining the registry doubles as the suppress signal: a cancelled background subagent's wait task sees its entry gone and drops its terminal delivery, so a stopped result can't repopulate the buffer. `/stop` stops only what's **running** — it deliberately leaves `pending_background_results` and any queued `BackgroundJobFinished` alone, so results from subagents that already *completed* still report normally once the cancelled turn returns. The ack lists each cancelled (running) background task by type + summary. `/stop` is published in every surface's slash list (gateway `MANIFEST`, web `/chat/slash-manifest`, TUI `commands()`) but `PassThrough` at each edge — execution is central.

**Non-obvious scheduling invariants (don't revert on intuition):**

- `ActorStop` is the **lowest** priority, not the highest — so cron's back-to-back `CronTrigger`→stop FIFO holds and a reaper stop never jumps ahead of a just-delivered `BackgroundJobFinished`. "Stop now" is `/stop`'s cancel path, never a mailbox tier.
- Automatic priority is **queue-ordering only, non-preemptive** — a running turn is never interrupted by a higher-priority arrival; `/stop` is the only explicit preemption.
- The notification framing lives in **per-turn content, never the system prompt** — moving it would change the cached prefix and break the prompt cache. This is why the turn reuses the exact main-path system prompt + toolset.
- There is **no `<no_output/>` sentinel** — the model isn't told it may stay silent; an empty final message is simply not sent.
- UserInput coalescing has **no debounce timer** (drains already-queued only — it does not batch rapid sends to an idle actor); every leading-slash message is a hard merge boundary, not just `/compact`.
- **No cron-vs-`BackgroundJobFinished` priority rule** — a cron session is one-shot and unregistered, so `BackgroundJobFinished` never reaches it.

### Conversation title

A fresh top-level user session gets a short **conversation title** summarizing the user's first question — the label the web chat renders in its header + sidebar row. `AgentLoop::maybe_generate_title` fires at the **start** of `run_inner` (right after the system prompt is seeded, before the first LLM call) and, when the gate holds, `tokio::spawn`s a **detached, fire-and-forget** pass — so the title is derived **concurrently with the turn's own answer** (it depends only on the question, already in context, not on the reply) and the user's reply never blocks on it. The pass is **not a job of its own**: it records a `StepKind::TitleGeneration` step + `LlmCall` span **under the triggering turn's own job** (`current_job_id`), so cost + trace attribute to that turn — exactly like the progress observer. It rides the **turn's `cancel_token`** (a `/stop` closes the title step as `Cancelled` cleanly via `with_step`; a normally-completed turn leaves the token untripped so the pass finishes even if it briefly outlives the reply — the title is cosmetic and self-heals on a later turn, so unlike the background-summary pass it needs no reap-surviving token). It runs `runtime::title::TitleRunner` (a lean sibling of the progress observer: `CallReason::Title`, no tools, over a fresh prompt built by `baybo_context::prompts::title` — it does **not** read the turn context), sanitizes the reply into a short title, persists it via the `Session.title` flat column (`SessionManager::set_title` — a targeted UPDATE that survives a concurrent `touch`, like `hidden`/`pinned`/`folder_id`), and notifies the loop's `SessionTitleSink` so the display surface can broadcast it live.

Gate (all must hold): the turn is `UserChat`; a `SessionTitleSink` is wired (the "a live title surface exists" signal — present in the running gateway, `None` in tests / headless, so titles are generated only where something renders them, and existing e2e turns don't pay for or race against a title pass); the session is a top-level user session (`TriggerSource::User`, no lineage — cron / subagent skipped); it has no title yet (`session.title.is_none()`, the durable once-only guard, self-healing across rehydration); and this actor hasn't already attempted one (`title_generation` handle present, the per-actor-lifetime guard). The title input is the first genuine user question — the first `MessageSource::User` transcript row that carries text ([`first_user_question`] skips a media-only opener and advances to the first text-bearing question); a first turn with no text-bearing user row leaves the session untitled. The sink is channel-agnostic: the gateway's `SessionTitleBroadcaster` (in `crates/gateway/src/channel/session_title.rs`, a sibling of the `SessionPulse` activity broadcaster) implements it by broadcasting a `Frame::SessionUpdated { patch.title }` on **every installed Subscribed channel** (web `http`, the iOS app, …) — the same channel-wide patch the pin / hide / folder mutations use — so whichever surface owns the session converges without a list refetch (non-Subscribed channels like Telegram have no patch surface and are skipped); the assembly layer (`crates/baybo/src/runtime.rs`) only constructs it and wires it into each actor's `AgentLoop`. See [`docs/web-chat.md`](../web-chat.md) → *Rename* for the client render (title → `last_user_text` → placeholder).

### LLM-invocable cron tools

`baybo_cron::tools::agent_tools` returns `CronCreateTool`, `CronDeleteTool`, and `CronListTool` — `Tool` trait implementations that let the LLM schedule/cancel/inspect cron jobs mid-conversation. They live in `baybo-cron::tools` (not `baybo-tools`) because they each hold `Arc<CronScheduler>`, and `baybo-tools` cannot depend on `baybo-cron` without creating a cycle. `crates/baybo/src/runtime.rs` registers them into the `ToolRegistry` after the scheduler is constructed.

### Startup recovery

On boot, `baybo_agent::recovery::recover_orphaned_traces_and_jobs` closes
half-open trace rows and cancels non-terminal jobs left by a prior process death
as `SystemCrash`. During the current process, `actor::runner::spawn_actor`
watches actor task panics and calls `recover_panicked_actor_session` for that
session's active turn jobs, then emits a user-facing crash notice. The TurnState
inactive edge still comes from the job lifecycle event via the projector, not
from the runner directly.

### Router's upstream responsibilities

Before a message enters an actor, Router completes: session identification/creation, user-level rate limiting, quota check via `CostManager::check`, select/create target `AgentActor`.

### Actor-side slash commands

`AgentActor::handle_user_input` inspects the leading text block of every inbound `IncomingMessage` for control slash commands before routing into `run_agent_loop`. Today the only one is `/compact`, which calls `AgentLoop::compact_now` — the method mints a job (matching the session's trigger kind), drives `ContextManager::force_compress` via the same `CompressionRunner` the iteration-top path uses, and returns the before/after-token confirmation text. The actor wraps that text as `AgentEvent::Notice { level: Info, ... }` rather than `Message` so the response renders out-of-band and stays out of the assistant transcript (the user typed a control command, not a question). Trailing arguments are ignored; matching is case-insensitive on the command token. Sidecar channels learn the command via the gateway slash manifest (`crates/gateway/src/channel/slash.rs::manifest`), but the gateway dispatcher passes it through unchanged — only `/new` needs server-side state.

### Per-session model selection

Each session can pin its own `baybo.json` LLM entry via `session.state.last_llm` (`None` ⇒ follow `default-llm`, so an un-switched session keeps tracking global default changes). The pin flows into the loop's `initial_llm`: at a cold spawn / post-eviction hydration, `Router::handle_incoming` reads `session.state.last_llm` and passes it to the actor spawner; for a **live** actor, `AgentMessage::SetModel { llm }` (Trigger-tier, so it lands at a turn boundary — never mid-turn) re-pins the loop in place via `AgentLoop::set_initial_llm`. Either way the swap takes effect on the **next** turn: `AgentLoop::refresh_active_llm` re-resolves `initial_llm` against the hot-swappable `LlmClientPool` at the top of every turn — the same hook that absorbs config hot-reloads — swapping the client and context-window budget when the resolved entry changes. A stranded pin (entry later removed from config) degrades safely: `LlmClientPool::resolve` falls back to the default with a `warn!`.

Persistence and the live re-pin are deliberately **split** to avoid a lost-update race. `last_llm` is a **flat `sessions` column**, not a JSON-blob field — exactly like `hidden` — written only by the targeted `SessionStore::set_last_llm` and omitted from `save`'s `DO UPDATE`, so a concurrent `touch` (which is a full-blob `get` + `save` fired on every inbound message) can't clobber it; `get` patches `Session.state.last_llm` from the column on read. The chat `PUT /v1/chat/sessions/{id}/model` validates the name against the pool, then (1) **persists** via `set_last_llm` synchronously — authoritative for any later spawn, and a storage failure surfaces as an error rather than a false 200 — and (2) routes `SetModel` to re-pin the live actor **in memory only** (the gateway holds an `AgentSupervisor` clone for this reach-the-live-actor hop, the same way `/stop` reaches one). `SetModel` does not itself persist. Subagent spawns are the other `initial_llm = Some(...)` path, pinning via `model_tier` instead.

### Timeouts and time limits

Consolidated reference for every time bound a turn can hit. Two structural facts come first, because they explain why most of the table is about tools and subprocesses rather than the loop itself:

- **A turn is bounded by step count, not a wall clock.** `agent.max_iterations` (default 1000, range 1–1000; `AgentConfig` in `baybo-config`, enforced in `config/src/validate.rs`) caps how many LLM↔tool iterations one turn may run. Cancellation is cooperative, checked at the iteration boundary (`/stop` is the only hard interrupt) — there is no per-turn timer.
- **The main LLM chat call has no Baybo-imposed wall-clock timeout.** The shared reqwest client (`baybo_security::http::client`) sets no `.timeout()`, so a `chat` / `chat_stream` call is bounded only by the provider/transport. Transient failures (5xx/408/429, connect/transport flake) are absorbed by the retry loop below, not by a deadline.

**LLM retry** — `ErrorHandler::default` in `runtime/error_recovery.rs`, wrapping every model call in `AgentLoop::call_llm`. Exponential backoff, capped; not configurable (hardcoded default).

| Knob | Value |
|------|-------|
| `max_retries` | 10 |
| `backoff_base` | 1s |
| `backoff_max` | 30s |

Backoff sequence is `1, 2, 4, 8, 16, 30, 30, 30, 30, 30` s — worst case ≈ 2.7 min of waiting before the call gives up. Only `LlmError::is_retriable()` errors (transient) and raw `io::Error` retry; config / model-shape errors surface immediately.

**Tool execution** (`runtime/tool_executor.rs`) — two nested deadlines:

- *Inner* = the tool's own `max_timeout()`, written into `ToolContext::timeout`.
- *Outer* = `ToolContext::timeout + APPROVAL_HEADROOM` (300s), enforced by `tokio::time::timeout`. The headroom mirrors the approval gate's wait window so a tool blocked on a user-approval prompt isn't killed before the user can answer.

Per-tool `max_timeout()`:

| Tool | `max_timeout` | Where |
|------|--------------|-------|
| trait default | 30s | `Tool::max_timeout` in `baybo-tools` (`tools/src/lib.rs`) |
| Bash | 600s | `tools/src/builtin/bash/mod.rs` — per-call `timeout_ms` and the sandbox spawn tighten further |
| WebFetch | 120s | `tools/src/builtin/web_fetch.rs` — connect phase capped at 10s independently |
| Grep / Glob | 60s | `tools/src/builtin/{grep,glob_tool}.rs` |
| send_local_file | 60s | `tools/src/builtin/send_local_file.rs` |
| Skill read (risk-assessed) | 60s | `skills/src/tools.rs` |
| Skill install pipeline | 120s | `skills/src/tools.rs` |
| MCP tool | 60s | `tools/src/mcp/tool.rs` |
| OpenViking memory store | 120s | `STORE_MAX_TIMEOUT` in `memory/src/backends/openviking.rs` |
| Subagent (in-process) | `TOOL_WAIT_BACKSTOP` = 30 days | `subagent/src/tool.rs` — effectively unbounded; the real bound is the caller's cancel / job lineage |

**Approval gate** — `APPROVAL_TIMEOUT` = 300s (`gateway/src/channel/boot.rs`). How long a tool-approval prompt waits for the user before timing out; the executor's `APPROVAL_HEADROOM` tracks it.

**Progress observer** (`runtime/progress_observer.rs`) — out-of-band status emitter for long UserChat turns:

| Const | Value | Meaning |
|-------|-------|---------|
| `OBSERVER_APPEAR_AFTER` | 10s | turn must run this long (and >1 iteration) before the first progress Notice |
| `OBSERVER_MIN_INTERVAL` | 40s | minimum gap between Notices — each is a billed LLM sub-call, so it stays sparse |

The observer fires from the loop's **`Continue` arm only** — after an iteration has resolved as a tool round, never on the one that produced the final answer — so a turn that just ended never spawns a fresh summary. At that point the context is coherent (tool results appended, no dangling `tool_use`) and still reuses that iteration's warm cached prefix. The summary is drained (emitted) at the *next* `Continue`. One residual remains: the last summary, spawned right before an iteration that turns out to be the final answer, can no longer be drained. To avoid that detached call lingering past the reply billed-and-discarded, the observer is bound to a dedicated `observer_cancel` child token (not the turn token): a drop guard trips it on **every** `run_inner` exit (Final / max-iter / error), and it inherits `/stop`; the observer's LLM call `select!`s on it, so an undrainable (or `/stop`-ed) summary aborts and closes its step as `Cancelled` instead of being `abort()`-ed (which would leak a `Pending` step). A summary that already finished before the turn ended is simply dropped.

**External CLI subagents** (`external_agent/*`, claude/codex/gemini) — opaque subprocesses, so they get real wall-clock guards:

| Const | Value | Meaning |
|-------|-------|---------|
| `EXTERNAL_SUBAGENT_TIMEOUT` | 8h | **idle** safety timeout; resets on every output line, kills only a silent/hung process |
| `VERSION_CHECK_TIMEOUT` | 5s | `--version` probe (`probe.rs`) |
| `KILL_GRACE` | 3s | SIGTERM→SIGKILL grace; `probe.rs` also waits `timeout(2s, child.wait())` for a graceful exit first |

**Actor lifecycle** (around the loop, not inside a turn):

| Const | Value | Meaning |
|-------|-------|---------|
| `REAP_INTERVAL` (`actor/supervisor.rs`) | 5 min | idle-reaper tick |
| `idle_timeout()` (`actor/supervisor.rs`) | 30 min | drop the in-memory actor after this much idle; the session row is never touched (see CLAUDE.md, "Session data is core data") |
| `NOTIFY_RETRY_INITIAL_BACKOFF` (`actor/mod.rs`) | 60s | subagent-completion notify retry, initial |
| `NOTIFY_RETRY_MAX_BACKOFF` (`actor/mod.rs`) | 300s | …capped at |

Router-level user rate limiting (`actor/router`) uses a sliding window (default 60s) — a time *window*, not a timeout.

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
| `model` | Provides `MessageSource::RecalledMemory` (the framed recall-injection marker); session domain types (`Session`, `User`, `ChannelType`) used by `baybo-session::SessionManager` |
| `memory` | Owns the pluggable `Memory` trait + `NoopMemory` default. The agent loop drives `recall` / `on_job_complete` for `UserChat` + `Cron` jobs; no real backend ships yet (runtime wires `None`) |
| `workspace` | Identity files for system prompt |
| `cron` | Owns `CronJob`, `CronExecution`, and `CronScheduler`; agent re-exports `CronScheduler` / `CronTriggerEvent` for assembly-layer wiring |
| `context` | Conversation window and compression |
| `job` | Owns `Job`, `JobStatus`, `JobInputKind` / `JobShape` (+ `Job.origin`), and `JobLifecycle` (persistence orchestrator + cancellation registry + lifecycle-event bus); the `JobStore` trait lives in `baybo-store` and this crate owns the `Job` ↔ `JobRow` conversions. Agent constructs and shares one `JobLifecycle` across the loop, router, supervisor, and subagent wait routine |
| `trace` | Owns `Step`, `Span`, `SpanEvent`, `SpanRecorder` (lifecycle facade), and `TraceEventStream` (broadcast bus); the `TraceStore` trait lives in `baybo-store` and this crate owns the row conversions. Agent constructs and shares one `SpanRecorder` per session |
| `query` | Owns `QueryApi` — the read-only analytics facade over session/job/trace/cost. Agent does not consume `QueryApi` directly; gateway and CLI do |
| `session` | Provides `SessionManager` and its error type (domain types live in `baybo-model`) |
| `security` | Provides crypto primitives, `SecretVault`, `SecretValue`, `LeakDetector`, `PlaceholderMinter`, `InjectionDetector`; `agent::security::SecurityGateway` composes them |
| `channels` | `Channel` handles + `ChannelRegistry`; Router owns the registry for dispatch by `ChannelType` |
| `store` | The ports crate: owns every `*Store` trait contract, the row/DTO types they exchange, and `StorageError`. Agent injects these trait objects |
| `storage` | Provides the libsql implementations of every `*Store` trait (the contracts all live in `baybo-store`) and bundles them in `Store` for DI |
