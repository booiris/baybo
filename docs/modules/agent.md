# agent - Assembly Layer and Execution Engine

## Overview

The `agent` crate is Baybo's top-level assembly layer, connecting all other modules into an executable engine.

Core responsibilities:

- **Message dispatch**: Actor model, one Actor per session for isolation
- **Agent main loop**: LLM calls, tool/skill execution, reply generation
- **Business logic managers**: `SessionManager` (in `baybo-session`), `TurnLifecycle` (in `baybo-turn`), `SpanRecorder` (in `baybo-trace`), the `Memory` trait (in `baybo-memory`), `CostManager` (in `baybo-cost`), `SecretVault` (in `baybo-security`), `SecurityGateway` — all domain managers live in their respective domain crates now; `agent` assembles them. `SecurityGateway` stays here because it is a cross-cutting interception facade tied to the execution path
- **Long-running execution**: cron scheduling, background notifications
- **Unified observability**: `SpanRecorder` (in `baybo-trace`, Step / Span / SpanEvent) and `TurnLifecycle` (in `baybo-turn`, Turn state machine)
- **Cost management**: `CostManager` (in `baybo-cost`) records LLM-call cost and gates spend; agent constructs it and threads it through the loop
- **Runtime logic**: error recovery, timeout control

It does not own low-level storage or backend implementation — it consumes every `*Store` trait from the `baybo-store` ports crate through dependency injection, and the sqlite impls (`baybo-storage`) are wired in at assembly time. Domain managers and rich types come from their respective crates (`session`, `model`, `trace`, `security`, `turn`, `cron`); the `TurnStore` / `TraceStore` it calls trade in row DTOs that `baybo-turn` / `baybo-trace` convert to and from. Each manager defines its own error type for business-level failures (e.g. `TurnLifecycle` defines errors for invalid state transitions).

## Source Layout

`src/` is split along two axes — per-turn execution and per-session actor orchestration — plus the cross-cutting policy / process-level infrastructure that lives outside either bucket:

```
agent/src/
├── lib.rs
├── security.rs               # SecurityGateway (cross-cutting interception facade)
├── service.rs                # ShutdownSignal, TaskTracker (process-level)
├── recovery.rs               # startup + actor-panic recovery
├── external_agent/           # claude / codex CLI subagents + version probe
├── runtime/                  # per-turn execution core
│   ├── agent_loop.rs         # AgentLoop, AgentLoopConfig
│   ├── tool_executor.rs      # ToolExecutor + approval gate; wires virtual-file providers into ToolContext
│   ├── virtual_read.rs       # SessionTranscriptReader: VirtualReadResolver serving the transcript (ReadTool consults it)
│   ├── compression.rs        # compaction LLM call: step/span, cost, retry
│   ├── billed_chat.rs        # cost-aware LLM call wrapper
│   ├── error_recovery.rs     # retry / degrade policy
│   ├── progress_ledger.rs    # per-turn file-mutation ledger; spots edits that cancelled out
│   ├── sandbox.rs            # SandboxAdapter glue for tool exec
│   ├── scope.rs              # with_turn / with_step / with_span / LLM span guards
│   ├── llm_pool.rs           # per-provider LlmClient pool
│   ├── background_jobs.rs    # backgrounded-Bash sink + detached escort
│   ├── progress_observer.rs  # out-of-band status emitter for long turns
│   ├── subagent_spawner.rs   # out-of-band subagent-spawn ingress
│   └── title.rs              # conversation-title pass
└── actor/                    # per-session actor + orchestration
    ├── mod.rs                # AgentActor + AgentMessage
    ├── background_notification.rs # durable background-result delivery pipeline
    ├── mailbox.rs            # bounded priority mailbox
    ├── runner.rs             # tokio task boundary + actor panic recovery
    ├── supervisor.rs         # AgentSupervisor + idle reaper
    ├── subagent.rs           # subagent wait routine
    ├── router/               # ingress dispatch (cron / user / output; subagent-spawn ingress lives in runtime/subagent_spawner.rs)
    └── state/                # DurableActorState + VolatileResources
```

For backwards compatibility, `lib.rs` re-exports the submodules at the crate root (`baybo_agent::agent_loop`, `baybo_agent::supervisor`, etc.), so consumers don't see the directory split unless they want to.

## Design Decisions

### Actor isolation model

One Actor per session: natural serialization within a session (no context races), natural concurrency across sessions. All control messages (timeout, cron) route to the same actor.

### Main execution path (AgentLoop)

1. System-prompt assembly lives in [`baybo_context::prompts::soul`](context.md) (`assemble` reads the bound agent's resolved persona files — flat for global/legacy agents, below `personas/project/` for newly created project agents, and `personas/baybo/` for the built-in and for an unbound session — plus the shared `personas/USER.md` and the `<memory>` index, and frames them with TOP/TAIL hints). `ContextManager` owns the whole system-prompt lifecycle: `ensure_seeded()` resolves the prompt (`resolve_system_prompt` — a subagent profile looked up by name in the registry, else the bound agent's own persona files, else a fallback), seeds the leading `System` row, and appends the skill reminder. Every `AgentLoop::append_*` entry point calls `ensure_seeded()` before appending its row.
   A seeded row is durable, so it long outlives the deploy that wrote it. Two seams correct that, both owned by `ContextManager` and both described in [context.md](context.md#the-system-prompts-lifecycle): `reconcile_system_prompt()` runs **before every main LLM call** (step 4 below) and appends the parts that have changed on disk, and the reseed-after-compaction rewrites the row itself. So a mid-session persona edit reaches the model on the next call as a delta, and stops being a delta at the next compaction.
2. Append current user message to Context
3. Skill selection (`ContextManager::invocable_skill_summaries()`): `SkillRegistry::summaries_for(skill_scope())` — the compiled-in builtins that scope may see (all of them for the built-in and for an unbound session, only `UNIVERSAL_SKILLS` for any other agent) layered with the bound agent's own `personas/<id>/skills/`, so per-agent scoping is the *first* filter — then `agent_invocable && trust_level != Untrusted && allows_channel(session.channel)`. This set backs the seed-time skill reminder and the post-compaction trailer; the per-turn `/command` candidate list is deliberately a *different* set (`slash_skill_summaries`: `command.is_some() && !Untrusted && channel-admitted`, independent of `agent_invocable`) so a slash-only skill like the builtin `deck` (`/deck`) stays user-invocable without ever being advertised to the model. Risk assessment fires later, inside `SkillTool` at invocation time (see `crates/skills/src/tools.rs`), not during selection — except an explicit user `/command`, which `ContextManager::expand_slash_command` treats as authorized and injects directly. The skill reminder is seeded once by `ensure_seeded` (and re-inserted after each compaction), not rebroadcast per turn.
4. Loop: `reconcile_system_prompt()` → `maybe_compress()` → build `ChatRequest` → call `LlmClient` → parse response → dispatch tool execution. The reconcile sits ahead of the compression gate for the same reason the task-checklist refresh does: whatever it appends rides this request, so the gate has to see its tokens.
5. Emit `OutgoingMessage` and persist Turn, Trace, and Cost state

### SpanRecorder lock strategy

`SpanRecorder` exposes short-lived `begin_step`/`end_step` and `begin_span`/`end_span` (closed with a `LifecycleOutcome`). `AgentLoop` and `ToolExecutor` must never hold locks while waiting for LLM calls or tool execution.

Tool spans use `with_span`: `ToolExecutor` closes the span during execution with
its result — either a `ToolCallOutput::Persisted` pointer keyed on the call's
begin-time `tool_use_id`, or a smaller inline copy. Errors close immediately with
`SpanFinalize::Empty`. The pointer resolves on read against the `ToolResult` row
`AgentLoop` appends afterward.

### ToolExecutor responsibility

ToolExecutor: lookup tool → validate trust/capability → consult approval gate → construct `ToolContext` → create child Turn/Trace nodes → reveal placeholders in args → execute → sanitize output → write results. It does **not** decide whether a tool should be called — that's `AgentLoop`.

`ToolExecutor` holds an `Arc<SecurityGateway>`. Tool invocation is the one legitimate plaintext boundary for arguments: the pre-reveal `params` is what flows into `SpanKind::ToolCall`'s `ToolCallBegin.params` and the approval preview (placeholder form), while a cloned `params_revealed` — with `reveal_in_value` applied — is what's passed to `tool_registry.execute`. After execution the returned `ToolOutput` is run through `sanitize_tool_output` so any tool-echoed secret is re-minted and vaulted before it enters the trace, the next LLM call, or memory. Errors are passed through `sanitize_error` before the error bubbles into `with_span`, which closes the span as `Failed { reason }`.

### Sandbox scope per call

For a tool declaring `ExecCommand`, `ToolExecutor` builds a fresh
`SandboxAdapter` (`runtime/sandbox.rs`). `permissive_scope` derives the
filesystem scope: `$HOME` as the one extra RW root (falling back to the
sandbox FS root when there is no `$HOME`), and a mask over **both** baybo
state roots — the live workspace (`WorkspacePaths::root()`, i.e.
`config.workspace.path`) and `$BAYBO_HOME` / `~/.baybo`. They diverge as soon
as an operator moves the workspace, and masking only the env-var location left
the live `state/storage.db`, `.key/`, `config/` and `personas/` readable *and*
writable inside the `$HOME` bind. On top of that: the caller's own
`personas/<id>/skills` and `state/blobs` read-only
(`shell_reachable_workspace_roots`), and an issue run's checkout read-write.

### LLM-response defensive scrubbing

`AgentLoop` holds an `Arc<SecurityGateway>`. In `call_llm`, every `LlmResponse` — including `content`, `content_blocks` text, `thinking`, and `tool_calls[*].arguments` — is run through `SecurityGateway::sanitize_llm_response` *before* the response is recorded to the trace or appended to `session.messages`. This prevents LLM-fabricated secret-shaped strings from leaking into any downstream sink.

### Tool-result formatting into LLM context

After `ToolExecutor::execute` returns, `AgentLoop` renders the result into a text blob (`ToolOutput::Text` → raw; `ToolOutput::Json` → serialized; `ToolOutput::Error` and errors → a prefixed error line), then bridges the **detect/format split**: `context_manager.cap_tool_output` first (caps to `MAX_TOOL_OUTPUT_BYTES`, spilling oversize payloads under the workspace's tool-spills dir so the truncation notice lands inside the envelope), then `SecurityGateway::detect_injection` (the scan stays in `baybo-security`), then `baybo_model::wrap_tool_output(&tool_name, &capped, &warning_rules)` (the `<tool_output>` envelope + breakout-escape + injection banner). The wrapped string populates `ContentBlock::ToolResult { content }`. The cap lives in `baybo-context`, the scan in `baybo-security`, and the envelope in `baybo-model` — deliberately, because `baybo-tools` frames its judge prompts with the same wrapper and cannot depend on `baybo-context`. The shared `</tool_output>` delimiter is `baybo_model::TOOL_OUTPUT_{OPEN,CLOSE}_PREFIX`, which the wrapper sits beside so the two cannot drift apart.

The tool span's copy of this result does not re-store the wrapped body: for a
larger result the span holds a `ToolCallOutput::Persisted` pointer to the
`ToolResult` row (by `tool_use_id`) that `ContextManager::append` writes here, so
the payload lives once. Smaller results stay inline when that serializes smaller.
See [trace.md](trace.md#toolcall-output-storage).

### Streaming delta reveal

`AgentLoop::chat_streaming` is the only path that emits plaintext secrets. Raw chunks accumulate into a `pending` buffer; `safe_flush_boundary` returns the largest prefix that cannot contain a partial placeholder (last unmatched `[{`, or a lone trailing `[`). Buffer size is capped at `STREAM_BUFFER_HIGH_WATER = 128` bytes to force flushes under pathological input. The flushable prefix is scanned/minted/vaulted once; the placeholder form is appended to the `LlmResponse.content` accumulator that the caller returns (so trace and memory see placeholders), while `reveal_in_text` is applied to the copy sent to `delta_tx` for user-facing display.

### Approval gate wiring

`ToolExecutor` holds an `Arc<ApprovalGateMap>` shared with `ChannelRegistry`. The map is populated automatically when channels register — `ChannelRegistry::register` reads `Channel::approval_gate()` and inserts the returned `Arc<dyn ApprovalGate>` keyed by the channel's `ChannelType`, and evicts it on `unregister`. For every call:

1. Resolve the gate for the session's channel via `gate_map.get(user.channel)`.
2. Compute `ResourceAccess` list via the tool's `accessed_resources(params)`.
3. Filter out entries covered by the snapshot of `SessionState::approved_resources` passed in from `AgentLoop`.
4. If any remain, call `gate.request(...)` with the uncovered set and a truncated params preview. The gate returns an `ApprovalOutcome`, not a bare decision; on `Deny` the call short-circuits to `ToolError::Denied` (recorded on the trace before return) whose `reason` is worded from `baybo_tools::refusal_reason(outcome.resolution)`. That distinction is the point: a 300 s window nobody answered, a prompt torn down by a cancel, and a standing policy are not a human refusal, and reporting them to the model as one teaches it to re-argue with somebody who was never there. A cancel that fires while the prompt is still up records `Abandoned` — nobody decided anything — rather than being written down as a decision.
5. On `ApproveAlways`, the executor de-dupes and pushes the newly-approved accesses directly into the shared `Mutex<Vec<ApprovedResource>>` passed by `AgentLoop`. After all tool calls complete, `AgentLoop` flushes the contents back into `session.state.approved_resources`, which persists through session save/restore because the types live in `baybo-model`.

Parallel tool calls within a turn each go through the gate independently; the gate implementation is responsible for its own serialization (TUI queues and shows one inline prompt at a time).

### Long-running model

Cron jobs flow through the Actor model and observability chain: `CronScheduler` → `Router` → `AgentSupervisor` → `AgentMessage::CronTrigger` → `AgentLoop`. All create Turn and Trace records. Background results are delivered asynchronously without polluting foreground conversation. Cron jobs are bound to `user_id + channel` (not `session_id`) so they survive session expiration; sessions are resolved dynamically at trigger time.

`AgentMessage::CronTrigger { job_id, prompt, delivery }` carries the cron job id, the prompt, and where the reply goes. `AgentActor` dispatches `prompt` through `dispatch_cron_prompt` with `TurnInput::Cron`, which appends the fire via `AgentLoop::append_cron_fire` (framed by `baybo_context::prompts::cron`, so it reads as a task, not a user message) and runs the normal `AgentLoop` path; the LLM decides what tools (if any) to invoke. A hard-failed fire persists an error control event in its own session, so the conversation doesn't just look blank.

`delivery` (`CronDelivery`) splits the two cron shapes:

- **`Channel`** — a **recurring** fire. Its session is a first-class conversation (titled, listed, replyable), so the reply dispatches out through the channel as usual: the conversation *is* the notification.
- **`OriginSession`** — a **one-shot** fire. Its session is a private workspace that emits nothing; the result is delivered into the conversation that scheduled the job. A waiter in `actor/router/cron.rs` (subscribed to the lifecycle bus *before* the trigger is sent) picks the outcome off the fire turn's terminal edge, stamps it on the `CronExecution` delivery ledger, and hands it to the origin as `AgentMessage::CronResultReady`.

`AgentMessage::CronResultReady` is handled at a turn boundary with **no inference** (`handle_cron_result_ready`): un-hide the conversation if the user had removed it, read the fire's reply row, and atomically append it under `cron-execution:<execution_id>` as a `MessageSource::CronNotification` assistant row framed with a scheduled-task header. The `(session_id, source_event_id)` unique index turns a boot replay into an existing-row result, so the actor skips the duplicate turn/channel dispatch and only resolves the ledger. A new row opens a `TurnInput::CronNotification` turn whose `Completed { reply_ordinal }` edge drives the push preview. Every outcome creates a notification row, including failure and an empty reply. This exactly-once guarantee stops at the transcript boundary; turn/push/channel delivery is not part of that database transaction. See [`cron.md`](cron.md) for the full delivery contract.

### Background jobs

Whether a turn may **create** background work — a `Bash` command that converts to background on timeout, or a subagent that converts (or is dispatched with `background=true`) instead of blocking — is one bit decided once per turn by `runtime::background_jobs::background_eligible` (defined next to the sink both consumers feed, not in either consumer), and it has two halves:

- **Session**: `Session::can_host_background_jobs` — a background result is delivered by an autonomous notification turn, so the session has to be a live, registered conversation the user can open. Top-level `TriggerSource::User` sessions qualify, and so does a **recurring cron fire's own conversation** (`TriggerSource::is_cron_conversation`): it is listed, replyable, pushable, and its actor is registered with the supervisor exactly so a reply reaches it (see [`cron.md`](cron.md)). A **one-shot** fire's workspace is invisible and deliberately unregistered, and a **subagent** session's turn ends with the child; both are out.
- **Turn**: the turn must not be a cron fire's own `TurnInput::Cron` turn. A fire that backgrounded its slow work would notify with a partial report and deliver the real answer as a separate turn later, which defeats the point of a scheduled report — so a fire blocks until its work is done, with no foreground-wait timer at all.

The pair is what lets a recurring job's conversation behave differently by turn: the **fire blocks**, and a **user reply in the same conversation backgrounds** like any other chat. Everything else keeps its prior behaviour — in particular a background result's own notification turn stays eligible, so the agent reacting to one job may dispatch the next.

The bit reaches the work through `ToolContext::background_eligible` (Bash reads it directly; `spawn_subagent` forwards it on `SubagentParentContext` so the spawner does not re-derive the session half and miss the turn half). `background_jobs` / `background_control` on the same context are *capability* handles, `None` only where no manager is wired — never a policy switch. Observing existing jobs is not creating work, so `JobList` / `JobStop` ignore the gate entirely.

### Background-result notifications

Detached subagents and detached `Bash` commands share one durable notification pipeline, implemented in `actor/background_notification.rs`. `SessionState::background_notifications` owns three explicit stages: grouped results waiting on a barrier, completed results buffered before transcript commit, and one active transcript-backed delivery ledger. The active delivery and fresh buffered results may coexist; the older batch always settles first.

Detached command escorts also select the process-wide shutdown token. Shutdown kills the child and clears its process registry and ledger without publishing `BackgroundJobFinished`; partial stdout/stderr remains available for inspection. A command terminated because Baybo is stopping is not reported as a task failure and never enters the durable notification aggregate.

Delivery is append-first and forward-only. For each batch the actor first persists and dispatches a deterministic assistant reply saying the background work finished, including the bounded `summary_text`; it then persists the hidden `<background_results>` prompt under a separate deterministic source-event key. Once the delivery ledger is durable, the parent runs a normal streaming turn: `Reasoning`, tool progress, and `AnswerDelta` events are visible before the canonical final `Message`. Failed turns retain their transcript rows and retry from the ledger on a timed exponential backoff; compaction re-anchors have their own deterministic operation key, so a crash replay recovers the prior ordinal instead of duplicating the hidden prompt. The retry cue — the synthetic user-role tail that keeps a request from ending on a cancelled attempt's assistant salvage (provider prefill, which Anthropic rejects with extended thinking on) — is **not** a transcript row: it is a request-time suffix applied only while the tail is actually an assistant row, so it is recomputed per request (a persisted, attempt-keyed cue was a no-op on the exact crash-replay it had to survive) and rides the trace marker so replay matches what the model saw. Successful user turns can settle an open delivery passively, and the attempt cap also degrades to passive delivery because the prompt remains durable. Actor eviction preserves every stage in the session row. See [`docs/background-notifications.md`](../background-notifications.md) for the complete intake, grouping, scheduling, persistence, retry, `/stop`, compaction, and crash contract.

The per-session mailbox is a **priority queue** (`mailbox::channel`): `UserInput`/trigger > `BackgroundJobFinished` / `CronResultReady` > `ActorStop`. A rapid burst of `UserInput`s coalesces into one turn; a leading `/command` is a hard boundary.

**Mid-turn user interjection (steering).** A message the user sends *while a `UserChat` turn is running* is injected into that turn at the next tool boundary — drained from the mailbox at the top of each loop iteration after the first (before `compress_if_needed`, never mid-call), framed with a `<user_interjection>` steering envelope, and appended before the next LLM call. The loop reaches the mailbox through the `runtime::agent_loop::InterjectionSource` seam, which `AgentActor` implements over its `MailboxReceiver` (`MailboxInterjections`) using `MailboxReceiver::try_recv_if` to pop only the leading run of **non-slash** `UserInput`s — a queued slash command / `BackgroundJobFinished` / `ActorStop` stops the drain and is left for normal dispatch. Only `handle_merged_user_turn` (the non-slash user path) passes the source; cron / subagent-spawned / `/skill` / notification turns pass `None`. Each drained message is persisted as a faithful `MessageSource::UserInterjection` row (a clean user bubble — `from_user()` is true for it); the envelope is applied **wire-only** in `ContextManager::messages_for_llm` (`frame_interjections`, re-derived each call so it survives compaction). Non-preemptive: the in-flight tool/LLM call is never cancelled (`/stop` remains the only hard interrupt), and a message that never reaches a tool boundary (e.g. the turn ends with a `Final` response, or iteration 1 produced no tool calls) falls through to the next turn. See `docs/mid-turn-user-interjection.md`.

**`/stop`** is an out-of-band control command recognised in `Router::handle_incoming` (not the actor — a busy actor can't read its mailbox to preempt its own turn; the `@BotName` group-command suffix is stripped, mirroring the gateway slash parser). It cancels the session's in-flight turn + every in-flight subagent (foreground via turn lineage `TurnLifecycle::list_children`, background via the supervisor's `in_flight_background_subagents` registry). Background subagents are stopped by **cancelling the child's `CancellationToken`** — stored in the registry at dispatch, so this works even in the window *before* the child's turn row exists (a turn-store lookup would miss it and let it run on); the turn is also cancelled `UserStopped` best-effort for audit when the row exists. Draining the registry doubles as the suppress signal: a cancelled background subagent's wait task sees its entry gone and drops its terminal delivery, so a stopped result can't repopulate the buffer. `/stop` stops only what's **running** — it deliberately leaves `session.state.background_notifications` and any queued `BackgroundJobFinished` alone, so results from subagents that already *completed* still report normally once the cancelled turn returns. The ack lists each cancelled (running) background task by type + summary. `/stop` is published in every surface's slash list (gateway `MANIFEST`, web `/chat/slash-manifest`, TUI `commands()`) but `PassThrough` at each edge — execution is central.

**Where a cancel actually lands.** The iteration boundary is a backstop, not the only observation point. `AgentLoop` observes the turn's cancel token at three places, and the middle one is what makes `/stop` land in seconds:

1. **During the LLM call** — `call_llm` `select!`s the non-streaming `chat()` against the token and threads it into `chat_streaming`, returning `CancelledTurn` with whatever partial text/reasoning was produced (persisted, so a reload still shows the turn's work).
2. **During the tool batch** — the batch is *raced* against the token rather than awaited to completion. The dispatched calls go into a `FuturesUnordered` drained under a `biased` `select!` whose stream arm is polled first, so every result already returned is kept; when the token trips, the remaining futures are dropped, which is what actually unblocks (a Rust future cancels on drop). This matters because most tools do **not** watch `ctx.cancellation_token` — only `Bash`, `Grep` and `WebFetch` do — so an MCP call or a `spawn_subagent` would otherwise hold the whole turn until its own timeout.
3. **At the iteration boundary** — the pre-existing check, which still catches the orchestration-layer wait windows.

Every call abandoned by (2) still gets a synthetic `tool_result` (`baybo_context::prompts::cancelled_turn::TOOL_RESULT_BODY`) appended in declaration order, because the assistant row carrying its `tool_use` was already persisted and a provider rejects a dangling id outright. `transcript_repair::repair_tool_pairing` only runs on cold hydration, so leaving the hole would wedge the live in-memory window for the life of the actor. The fill is deliberately weaker than the crash fill: a cancelled call's side effects may or may not have landed, and it is told so. A cancel that arrives between the LLM returning and dispatch bails *before* the assistant row is appended, so no `tool_use` is ever persisted for a batch that will not run; a final answer that beat the cancel is still delivered, and the prose the model wrote alongside the undispatched calls is salvaged through the same `salvage_partial_blocks` → `persist_cancelled_partial` pair (1) uses, so the work block does not empty itself depending on which microsecond the cancel landed. Grants from calls that did complete are flushed into `SessionState::approved_resources` either way, and the unwind returns `Err` through `with_step`, which carries the turn's token and therefore closes the step as `Cancelled` rather than `Failed`.

**The dropped futures in (2) still close their trace spans.** `ToolExecutor` opens the tool span before it does anything else, so a dropped call has a `Pending` row with `ended_at = NULL` already persisted — and nothing else would ever revisit it: `recovery` reaches pending spans only under a step that is itself *unfinished*, while the step around this batch closes normally on the same unwind. `runtime::scope::with_span` therefore carries a Drop guard (`SpanCloseOnDrop`) that closes the row as `Cancelled { reason }` from a spawned task when the guarded future is dropped instead of returning. Steps and turns need no such guard: dropping them leaves a non-terminal row, which is precisely what the boot sweep looks for.

**Residual:** dropping a `spawn_subagent` future stops the parent *waiting*, not the child *running*. `Router::handle_stop` cancels in-flight subagents separately (turn lineage + the background registry), so the user-facing `/stop` path is covered; a cancel from any other source (idle reaper, shutdown) leaves the child to finish.

**Non-obvious scheduling invariants (don't revert on intuition):**

- `ActorStop` is the **lowest** priority, not the highest — so cron's back-to-back `CronTrigger`→stop FIFO holds and a reaper stop never jumps ahead of a just-delivered `BackgroundJobFinished`. "Stop now" is `/stop`'s cancel path, never a mailbox tier.
- Automatic priority is **queue-ordering only, non-preemptive** — a running turn is never interrupted by a higher-priority arrival; `/stop` is the only explicit preemption.
- The notification framing lives in **per-turn content, never the system prompt** — moving it would change the cached prefix and break the prompt cache. This is why the turn reuses the exact main-path system prompt + toolset.
- There is **no `<no_output/>` sentinel** — the model isn't told it may stay silent; an empty analysed report is simply not sent after the already-delivered completion reply.
- UserInput coalescing has **no debounce timer** (drains already-queued only — it does not batch rapid sends to an idle actor); every leading-slash message is a hard merge boundary, not just `/compact`.
- **No cron-vs-`BackgroundJobFinished` priority rule** — a cron session is one-shot and unregistered, so `BackgroundJobFinished` never reaches it.

### Conversation title

A fresh top-level user session gets a short **conversation title** summarizing the user's first question — the label the web chat renders in its header + sidebar row. `AgentLoop::maybe_generate_title` fires at the **start** of `run_inner` (right after the system prompt is seeded, before the first LLM call) and, when the gate holds, `tokio::spawn`s a **detached, fire-and-forget** pass — so the title is derived **concurrently with the turn's own answer** (it depends only on the question, already in context, not on the reply) and the user's reply never blocks on it. The pass is **not a turn of its own**: it records a `StepKind::TitleGeneration` step + `LlmCall` span **under the triggering turn's own row** (`current_turn_id`), so cost + trace attribute to that turn — exactly like the progress observer. It rides the **turn's `cancel_token`** (a `/stop` closes the title step as `Cancelled` cleanly via `with_step`; a normally-completed turn leaves the token untripped so the pass finishes even if it briefly outlives the reply — the title is cosmetic and self-heals on a later turn, so unlike the background-summary pass it needs no reap-surviving token). It runs `runtime::title::TitleRunner` (a lean sibling of the progress observer: `CallReason::Title`, no tools, over a fresh prompt built by `baybo_context::prompts::title` — it does **not** read the turn context), sanitizes the reply into a short title, persists it via the `Session.title` flat column (`SessionManager::set_title_if_absent` — a targeted `UPDATE … WHERE title IS NULL` that survives a concurrent `touch`, like `hidden`/`pinned`/`folder_id`), and, **only when that write actually landed**, notifies the loop's `SessionTitleSink` so the display surface can broadcast it live.

Gate (all must hold): the turn is `UserChat`; a `SessionTitleSink` is wired (the "a live title surface exists" signal — present in the running gateway, `None` in tests / headless, so titles are generated only where something renders them, and existing e2e turns don't pay for or race against a title pass); the session is a top-level user session (`TriggerSource::User`, no lineage — cron / subagent skipped); it has no title yet (`session.title.is_none()`); and this actor hasn't already attempted one (`title_generation` handle present, the per-actor-lifetime guard). That `session.title.is_none()` arm is only a **pre-filter**, not the guard: it reads the actor's long-lived `Session` snapshot, which a rename (a targeted column UPDATE issued by the gateway) never refreshes, and it is evaluated seconds before the pass writes. The authoritative guard is the conditional write itself — `set_title_if_absent` — which is why a user who renames a brand-new conversation mid-turn keeps their name, and why no generated title is broadcast in that case. Losing that race costs one lite-model call and nothing else. `SessionManager::set_title` (unconditional) is reserved for the user-initiated rename path, so no machine writer can overwrite a title anywhere in the workspace.

Note that patch delivery carries no version or sequence, so in the microseconds-wide interleaving where the auto write commits, a rename commits, and only then the auto broadcast is sent, a peer tab can briefly show the generated title while the database holds the user's. It heals on that tab's next list read (reconnect, `Gap`, or refetch); the durable value is never wrong.

The title input is the first genuine user question — the first `MessageSource::User` transcript row that carries text ([`first_user_question`] skips a media-only opener and advances to the first text-bearing question); a first turn with no text-bearing user row leaves the session untitled. The sink is channel-agnostic: the gateway's `SessionTitleBroadcaster` (in `crates/gateway/src/channel/session_title.rs`, a sibling of the `SessionPulse` activity broadcaster) implements it by broadcasting a `Frame::SessionUpdated { patch.title }` on **every installed Subscribed channel** (web `http`, the iOS app, …) — the same channel-wide patch the pin / hide / folder mutations use — so whichever surface owns the session converges without a list refetch (non-Subscribed channels like Telegram have no patch surface and are skipped); the assembly layer (`crates/baybo/src/runtime.rs`) only constructs it and wires it into each actor's `AgentLoop`. See [`docs/web-chat.md`](../web-chat.md) → *Rename* for the client render (title → `last_user_text` → placeholder) and for the user-facing rename (`PUT /v1/chat/sessions/{session_id}/title`).

### LLM-invocable cron tools

`baybo_cron::tools::agent_tools` returns `CronCreateTool`, `CronUpdateTool`, `CronDeleteTool`, `CronPauseTool`, `CronResumeTool`, and `CronListTool` — `Tool` trait implementations that let the LLM schedule, edit, pause, resume, cancel and inspect cron jobs mid-conversation. They live in `baybo-cron::tools` (not `baybo-tools`) because they each hold `Arc<CronScheduler>`, and `baybo-tools` cannot depend on `baybo-cron` without creating a cycle. `crates/baybo/src/runtime.rs` registers them into the `ToolRegistry` after the scheduler is constructed.

### Startup recovery

On boot, `baybo_agent::recovery::recover_orphaned_traces_and_turns` closes
half-open trace rows and cancels non-terminal turns left by a prior process death
as `SystemCrash`. It also closes half-open detached trace rows under terminal
turns (for example a title-generation step that outlived its turn) without
changing that terminal turn. During the current process,
`actor::runner::spawn_actor` watches actor task panics and calls
`recover_panicked_actor_session` for that session's active chat turns, then emits
a user-facing crash notice. The TurnState inactive edge still comes from the turn
lifecycle event via the projector, not from the runner directly.

The transcript side of a crash heals lazily at hydration, not at boot: a death
mid-tool-batch persists an assistant row whose `ToolUse` ids have no
`ToolResult` (the loop appends results per call as they complete), and strict
providers reject any request built from that shape. When the actor is next
rebuilt, `ContextManager::restore_from_store` runs
`baybo-context`'s `transcript_repair::repair_tool_pairing`: dangling ids get a
persisted synthetic "interrupted" `ToolResult` (append-only), and displaced
result rows are repositioned adjacent to their issuing assistant row in the
in-memory window. The reverse crash tear is also contained: an orphan or
duplicate `ToolResult` is quarantined from that provider-facing window and
logged with its call id, while its durable transcript row remains untouched.
The streaming twin of the dangling-use guard is
`salvage_partial_blocks`, which drops streamed-but-undispatched `ToolUse` on
mid-stream cancel so the dangling row is never persisted in the first place.

### Router's upstream responsibilities

Before a message enters an actor, Router completes: session identification/creation, user-level rate limiting, quota check via `CostManager::check`, select/create target `AgentActor`.

### Actor-side slash commands

`AgentActor::handle_user_input` inspects the leading text block of every inbound `IncomingMessage` for control slash commands before routing into `run_agent_loop`. Today the only one is `/compact`, which calls `AgentLoop::compact_now` — the method mints a turn (matching the session's trigger kind), drives `ContextManager::force_compress` via the same `CompressionRunner` the iteration-top path uses, and returns a `CompactionNotice` (severity + text). The actor wraps it as `AgentEvent::Notice` rather than `Message` so the response renders out-of-band and stays out of the assistant transcript (the user typed a control command, not a question). The severity travels with the text because one outcome is not a confirmation: a summariser failure comes back `Warn` — nothing was compacted, and nothing was dropped to fake it — and is persisted as a `notice_warn` control event so a reload still shows it as a warning. Trailing arguments are ignored; matching is case-insensitive on the command token. Sidecar channels learn the command via the gateway slash manifest (`crates/gateway/src/channel/slash.rs::manifest`), but the gateway dispatcher passes it through unchanged — only `/new` needs server-side state.

### Per-session model selection

Each session can pin its own `baybo.json` LLM entry via `session.state.last_llm` (`None` ⇒ follow `default-llm`, so an un-switched session keeps tracking global default changes). The pin flows into the loop's `initial_llm`: at a cold spawn / post-eviction hydration, `Router::handle_incoming` reads `session.state.last_llm` and passes it to the actor spawner; for a **live** actor, `AgentMessage::SetModel { llm }` (Trigger-tier, so it lands at a turn boundary — never mid-turn) re-pins the loop in place via `AgentLoop::set_initial_llm`. Either way the swap takes effect on the **next** turn: `AgentLoop::refresh_active_llm` re-resolves `initial_llm` against the hot-swappable `LlmClientPool` at the top of every turn — the same hook that absorbs config hot-reloads — swapping the client and context-window budget when the resolved entry changes. A stranded pin (entry later removed from config) degrades safely: `LlmClientPool::resolve` falls back to the default with a `warn!`.

Persistence and the live re-pin are deliberately **split** to avoid a lost-update race. `last_llm` is a **flat `sessions` column**, not a JSON-blob field — exactly like `hidden` — written only by the targeted `SessionStore::set_last_llm` and omitted from `save`'s `DO UPDATE`, so a concurrent `touch` (which is a full-blob `get` + `save` fired on every inbound message) can't clobber it; `get` patches `Session.state.last_llm` from the column on read. The chat `PUT /v1/chat/sessions/{id}/model` validates the name against the pool, then (1) **persists** via `set_last_llm` synchronously — authoritative for any later spawn, and a storage failure surfaces as an error rather than a false 200 — and (2) routes `SetModel` to re-pin the live actor **in memory only** (the gateway holds an `AgentSupervisor` clone for this reach-the-live-actor hop, the same way `/stop` reaches one). `SetModel` does not itself persist. Subagent spawns are the other `initial_llm = Some(...)` path, pinning via `model_tier` instead.

### Timeouts and time limits

Consolidated reference for every time bound a turn can hit. Two structural facts come first, because they explain why most of the table is about tools and subprocesses rather than the loop itself:

- **A turn is bounded by step count, not a wall clock.** `agent.max_iterations` (default 1000, range 1–1000; `AgentConfig` in `baybo-config`, enforced in `config/src/validate.rs`) caps how many LLM↔tool iterations one turn may run. Cancellation is cooperative and observed inside the LLM call, inside the tool batch, and at the iteration boundary (`/stop` is the only hard interrupt — see *Where a cancel actually lands* above) — there is no per-turn timer. At 1000 this is a runaway-cost backstop, not a loop detector: a turn can churn for dozens of iterations well inside it (see *No-progress detection* below).
- **The main LLM chat call has no Baybo-imposed wall-clock timeout.** The shared reqwest client (`baybo_security::http::client`) sets no `.timeout()`, so a `chat` / `chat_stream` call is bounded only by the provider/transport. Transient failures (5xx/408/429, connect/transport flake) are absorbed by the retry loop below, not by a deadline.

**LLM retry** — `ErrorHandler::default` in `runtime/error_recovery.rs`, wrapping every model call in `AgentLoop::call_llm`. Exponential backoff, capped; not configurable (hardcoded default).

| Knob | Value |
|------|-------|
| `max_retries` | 10 |
| `backoff_base` | 1s |
| `backoff_max` | 30s |

Backoff sequence is `1, 2, 4, 8, 16, 30, 30, 30, 30, 30` s — worst case ≈ 3 min of waiting before the call gives up. Only `LlmError::is_retriable()` errors (transient) and raw `io::Error` retry; config / model-shape errors surface immediately.

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
| attach_file | 60s | `tools/src/builtin/attach_file.rs` |
| Skill read (risk-assessed) | 60s | `skills/src/tools.rs` |
| Skill install pipeline | 120s | `skills/src/tools.rs` |
| MCP tool | 60s | `tools/src/mcp/tool.rs` |
| OpenViking memory store | 120s | `OpenVikingTimeouts::store_max` (default) in `memory/src/backends/openviking.rs` |
| Subagent (in-process) | `TOOL_WAIT_BACKSTOP` = 30 days | `subagent/src/tool.rs` — effectively unbounded; the real bound is the caller's cancel / turn lineage |

**Approval gate** — `APPROVAL_TIMEOUT` = 300s (`gateway/src/channel/boot.rs`). How long a tool-approval prompt waits for the user before timing out; the executor's `APPROVAL_HEADROOM` tracks it.

**Progress observer** (`runtime/progress_observer.rs`) — out-of-band status emitter for long UserChat turns:

| Const | Value | Meaning |
|-------|-------|---------|
| `OBSERVER_APPEAR_AFTER` | 10s | turn must run this long (and >1 iteration) before the first progress Notice |
| `OBSERVER_MIN_INTERVAL` | 40s | minimum gap between Notices — each is a billed LLM sub-call, so it stays sparse |

The observer fires from the loop's **`Continue` arm only** — after an iteration has resolved as a tool round, never on the one that produced the final answer — so a turn that just ended never spawns a fresh summary. At that point the context is coherent (tool results appended, no dangling `tool_use`) and still reuses that iteration's warm cached prefix. The summary is drained (emitted) at the *next* `Continue`. One residual remains: the last summary, spawned right before an iteration that turns out to be the final answer, can no longer be drained. To avoid that detached call lingering past the reply billed-and-discarded, the observer is bound to a dedicated `observer_cancel` child token (not the turn token): a drop guard trips it on **every** `run_inner` exit (Final / max-iter / error), and it inherits `/stop`; the observer's LLM call `select!`s on it, so an undrainable (or `/stop`-ed) summary aborts and closes its step as `Cancelled` instead of being `abort()`-ed (which would leak a `Pending` step). A summary that already finished before the turn ended is simply dropped.

**External CLI subagents** (`external_agent/*`, claude/codex) — opaque subprocesses, so they get real wall-clock guards:

Both CLIs are spawned through the runtime's `ProcessManager`. Cancellation,
timeout, stream EOF, task drop, process-wide shutdown, and crash recovery all
reap the full CLI process tree rather than only the direct `claude`/`codex`
process.

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
| `RETRY_INITIAL_BACKOFF` (`actor/background_notification.rs`) | 60s | background-result notification retry, initial |
| `RETRY_MAX_BACKOFF` (`actor/background_notification.rs`) | 300s | …capped at |

Router-level user rate limiting (`actor/router`) uses a sliding window (default 60s) — a time *window*, not a timeout.

### No-progress detection

`runtime/progress_ledger.rs` keeps a per-turn record of every `Edit` / `Write`, and tells the model when a turn's edits have stopped going anywhere. It exists because nothing else in the loop could: `max_iterations` is a cost backstop at 1000, `error_recovery` only classifies LLM/IO errors, the progress observer never writes back into the turn, and a denied tool call leaves no state the loop can reason over — so a turn could edit one file five times, net zero, and no part of the runtime would notice.

**What it compares.** Not file contents — `Edit` rejects `old_string == new_string`, so every applied edit changes the bytes, and `FileFingerprint` carries an mtime, so it moves even when the content comes back. What identifies churn is the *sequence of state transitions*: an `Edit` names both endpoints of its own transition, so an edit whose `new_string` reproduces an earlier edit's `old_string` has undone that edit. A `Write` names only its result, so results are compared to results. Only the two hashes are kept, never the payloads.

**Detection is not reporting.** A repeat or a revisit is a *churn signal*; nothing is said until a file has produced three. One is not evidence of a loop — undoing an edit you just made is a normal way to explore, and a lone A→B→A is indistinguishable from a flag toggled on to test and back off. The incident this was built from reached three on its fifth edit, one edit before the user asked what was going on.

Signals need not be consecutive — a short burst of work between two of them is still one file the turn is failing to move — but **three consecutive advancing edits clear the count**. Without that decay, two stray signals in iteration 3 would still be sitting there in iteration 400 waiting to convict an unrelated third. A refusal breaks the run without earning decay: it is not progress. The decay threshold cannot drop to one without losing the founding case, which had a single genuinely-new edit between its second and third signals; tests pin it from both sides.

| Signal / verdict | Recognised when |
|---|---|
| `AttemptRepeated` | The exact same transition is submitted again — including a denied call resubmitted verbatim. |
| `StateRevisited` | An applied mutation puts the file back in a state it already held this turn. |
| `Futile` | Three consecutive attempts on one file were all refused or failed. Counts its own consecutive run, independent of the churn threshold. |

The reported verdict names whichever signal crossed the threshold, so the observation describes what just happened rather than the accumulated total. At most one observation per file per turn.

**It injects, it does not stop.** The verdict renders (`baybo_context::prompts::no_progress`) into a transient tail row via `ContextManager::set_progress_observation`, which rides exactly one request and is then cleared — never persisted, so it cannot replay as history. Injection rather than enforcement because the runtime knows the *fact* (this file is back where it was) but not whether it is a mistake: a flag toggled on to test and off again is indistinguishable. A model reasoning correctly from a missing fact needs the fact, not a killed turn.

**Per-turn, deliberately.** The ledger is cleared at the top of every `run_inner`. "Change that back" is ordinary work when the user asks for it between turns; it is only churn inside one turn.

Bounded at 128 files × 64 attempts, **least-recently-touched evicted first**. Both numbers are sized against the failure mode rather than against memory — the whole structure is a few hundred KB at pathological worst case, next to an LLM context measured in megabytes — and the eviction order is load-bearing: a turn that sweeps a crate and *then* churns one file is exactly the case worth catching, and insertion-order eviction drops that file's history precisely because it was seen first.

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
| `memory` | Owns the pluggable `Memory` trait + `NoopMemory` default. The agent loop drives `recall` / `on_turn_complete` for `UserChat` + `Cron` turns; real backends (`mem0`, `openviking`) are built from `config.memory.provider` via `baybo_memory::boot::build_memory_backend`; the runtime wires `None` only when memory is disabled or `provider = noop` |
| `workspace` | Identity files for system prompt |
| `cron` | Owns `CronJob`, `CronExecution`, and `CronScheduler`; agent re-exports `CronScheduler` / `CronTriggerEvent` for assembly-layer wiring |
| `context` | Conversation window and compression |
| `turn` | Owns `Turn`, `TurnStatus`, `TurnInputKind` / `TurnInput` (+ `Turn.origin`), and `TurnLifecycle` (persistence orchestrator + cancellation registry + lifecycle-event bus); the `TurnStore` trait lives in `baybo-store` and this crate owns the `Turn` ↔ `TurnRow` conversions. Agent constructs and shares one `TurnLifecycle` across the loop, router, supervisor, and subagent wait routine |
| `trace` | Owns `Step`, `Span`, `SpanEvent`, `SpanRecorder` (lifecycle facade), and `TraceEventStream` (broadcast bus); the `TraceStore` trait lives in `baybo-store` and this crate owns the row conversions. Agent constructs and shares one `SpanRecorder` per session |
| `query` | Owns `QueryApi` — the read-only analytics facade over session/turn/trace/cost. Agent does not consume `QueryApi` directly; gateway and CLI do |
| `session` | Provides `SessionManager` and its error type (domain types live in `baybo-model`) |
| `security` | Provides crypto primitives, `SecretVault`, `SecretValue`, `LeakDetector`, `PlaceholderMinter`, `InjectionDetector`; `agent::security::SecurityGateway` composes them |
| `channels` | `Channel` handles + `ChannelRegistry`; Router owns the registry for dispatch by `ChannelType` |
| `store` | The ports crate: owns every `*Store` trait contract, the row/DTO types they exchange, and `StorageError`. Agent injects these trait objects |
| `storage` | Provides the sqlite implementations of every `*Store` trait (the contracts all live in `baybo-store`) and bundles them in `Store` for DI |
