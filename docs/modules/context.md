# context - Context Management

## Overview

The `context` crate owns the per-actor conversation state: the
transcript (`messages`), the token budget, and the hardcoded
compression flow. Persistence is wired in directly — every
`ContextManager` takes a bound `SessionId` + `Arc<SessionManager>` at
construction; `append` and the compression apply mirror to
`session_messages` through the `SessionManager` wrapper in
[`baybo-session`](session.md). Tests construct an in-memory store via
`baybo_session::test_support::MemorySessionStore` and pass it through
the same constructor — no separate "in-memory mode" exists.

Core responsibilities:

- **Sole owner of the transcript**: `ContextManager` holds `Vec<ChatMessage>` directly. `Session` (in `baybo-model`) carries only metadata (id, user, channel, lineage, soul binding, …). Ordinary `append` calls `persist_appended` (→ `SessionManager::append_session_message`); `append_idempotent` asks the store to atomically claim a `source_event_id` and mirrors the message into the live window only for `Inserted`, never `Existing`. Every successful compression calls `persist_compaction` (→ `SessionManager::apply_session_compaction`). Cold-start hydration via `restore_from_store` seeds the manager so an actor restart preserves the conversation; on load it runs `transcript_repair::repair_tool_pairing`, which persists a synthetic "interrupted" `ToolResult` for any `ToolUse` a crash left unanswered (append-only) and repositions displaced result rows next to their issuing assistant row, so a crash-torn transcript can't wedge the next request on provider tool-pairing validation.
- **Caller-driven compression**: `append()` is pure (push + budget update); the agent loop calls `maybe_compress()` at well-defined points so compression LLM cost can be recorded against the cost ledger
- **Token budget tracking**: track current token usage and remaining capacity via `TokenBudget`, anchored to the provider's authoritative `usage.input_tokens` between calls
- **Hardcoded compaction flow**: a single impl block on `ContextManager` — one blocking summariser call, assembled with the verbatim tail, truncating only if that call fails. No trait, no dispatch — every production session takes the same path.

**Goal**: ensure the context sent to the LLM never exceeds the model's context window while preserving the most valuable information.

## Architecture

```
ContextManager (struct)
├── TokenBudget       — pure state: max_tokens, threshold, current usage
├── Tokenizer         — trait, counts tokens (no model id — calibration
│                       is keyed by the model_id passed into maybe_compress)
│   └── TiktokenTokenizer — BPE impl via `tiktoken-rs` (cl100k_base / o200k_base)
├── TokenCalibration  — required, per-model EMA ratio of actual/estimate
│                       fed back from `AgentLoop::call_llm`
├── current_model     — Option<String>; written by maybe_compress, used as
│                       calibration key + baseline-invalidation trigger
├── workspace         — Arc<baybo_workspace::WorkspacePaths>; resolves
│                       the transcript-recovery pointer, the identity files
│                       (soul assembly), and the
│                       tool-spills dir (oversize tool output)
├── system_prompt     — resolved system prompt for the initial seed (workspace
│                       soul or a subagent profile override); reseed-after-
│                       compaction re-reads the workspace instead
├── compressor.rs     — impl ContextManager block: the compaction flow
│   ├── pre-flight gate    — NoOp if non_system.len() ≤ keep_recent
│   ├── summarize          — invoke ChatCallback with SUMMARIZE_INSTRUCTION
│   ├── assemble_summary   — [system + summary + verbatim recent slice],
│   │                        or summary-only when the slice would not shrink
│   ├── truncate fallback  — keep system + last keep_recent (summariser failed)
│   └── reseed_system_row  — re-read workspace soul on every apply
└── prompts/          — all model-facing framing text + pure builders
    ├── soul.rs            — assemble_from_workspace (TOP/TAIL hints + identity)
    ├── cron.rs            — frame_cron_prompt / original_cron_prompt
    ├── background_notification.rs — build_completion_reply +
    │                              build_notification_content
    │                              (<background_results> notification XML)
    ├── interjection.rs    — wrap_interjections (mid-turn steering envelope)
    ├── recalled_memory.rs — wrap_recalled_memories (recall envelope)
    ├── tasks.rs           — render_task_list (transient checklist reminder)
    ├── title.rs           — build_title_prompt (conversation-title pass)
    ├── cancelled_turn.rs  — /stop salvage marker (SUFFIX + strip_marker)
    ├── tool_output.rs     — cap_tool_output / spill (+ MAX cap)
    └── compression.rs     — SUMMARIZE_INSTRUCTION + CONTINUATION_INTRO/FOOTER
```

`prompts/` is the single home for every piece of text the runtime injects into
the LLM transcript. The pure builders are unit-testable on their own; both
`ContextManager` (`resolve_system_prompt` via `ensure_seeded`,
`cap_tool_output`, `reseed_system_row`) and the agent-loop seam
(`append_cron_fire`, `append_background_completion_reply_once`,
`append_background_notification_prompt_once`) call into them. The
injection *detection* for tool output stays in `baybo-security`, and the
`<tool_output>` envelope itself is `baybo_model::wrap_tool_output` — it sits
beside the `TOOL_OUTPUT_{OPEN,CLOSE}_PREFIX` delimiters it keys off, and out of
this crate because `baybo-tools` needs the same framing for its bash risk-judge
prompts and cannot depend on `baybo-context` (which depends on it). What stays
here is the byte-budget cap and the content-addressed spill.

**Key design choice**: `ContextManager` is a **concrete struct** with a **concrete compression flow**. Both the management logic (append, budget check) and the compression algorithm are invariant — no swappable strategy, no extension trait. Per-session paths flow through one shared `WorkspacePaths` handle.

### Compression is caller-driven

`append()` only pushes the message and updates the token budget — it does **not** auto-compress. The agent loop calls `maybe_compress()` at the top of every iteration; that's the single point where compression LLM calls happen and where their cost is recorded against the cost ledger.

This trade-off — losing the "impossible to forget" property of auto-compression — is deliberate: every compression that reaches Stage 2 spawns a billable LLM call, and the cost-recording context (`SpanRecorder`, `JobId`, `CostManager`) only exists at the agent-loop layer. Auto-compressing inside `append()` would silently bypass that recording.

```rust
// Append in any number of places without cost-recording overhead.
self.context_manager.append(&user_msg).await;

// Single explicit compression site at the top of each iteration.
self.compress_if_needed(session, span_recorder, job_id, &cancel_token, delta_tx.as_ref()).await?;
```

`maybe_compress` returns `Result<CompressionOutcome>`, a four-variant enum: `Compressed` (the transcript was replaced with a shorter list), `BelowThreshold` (budget under the configured compression threshold; only produced by `maybe_compress`), `StrategyDeclined` (the compressor's pre-flight gate fired — non-system message count already at or below `keep_recent`, so even truncate couldn't shrink), or `NoSavings` (the compressor produced a candidate slice but its post-tokenise total wasn't smaller than the original). The chat closure supplied by the agent loop is what opens the `StepKind::Compression` step + `SpanKind::LlmCall` span and records the call against the cost ledger.

`force_compress` is the same call without the budget gate, for caller-initiated passes (e.g. a user-typed `/compact` slash command). The pre-flight NoOp and non-shrinking applies still surface as `StrategyDeclined` / `NoSavings`; only the threshold check is bypassed, so a too-small conversation is still left alone rather than rewritten as a one-line summary.

## Design Decisions

### Separation of concerns

| Concern                              | Owner                              | Rationale                                                                                  |
| ------------------------------------ | ---------------------------------- | ------------------------------------------------------------------------------------------ |
| Token budget (how much room is left) | `TokenBudget`                      | Pure state; agent can query `budget().remaining()` for other decisions                     |
| When to compress                     | `ContextManager::maybe_compress`   | Caller (agent loop) triggers at the top of each iteration so cost recording can be wrapped |
| How to compress                      | `compressor.rs` impl block         | One hardcoded flow on `ContextManager`; no swappable strategy                              |
| Per-session paths                    | `Arc<WorkspacePaths>`              | Resolves the transcript-recovery pointer baked into the summary message                   |
| Token counting                       | `Tokenizer` trait                  | Trait and `TiktokenTokenizer` impl both live here; no LLM-SDK coupling                     |
| Calibration key (which model)        | `maybe_compress`'s `model_id` arg  | Caller passes the LLM id at compression time; `ContextManager` stores and reuses it        |

### The compaction flow

`ContextManager::run_compression_flow` (in `compressor.rs`) is `async` and receives a one-shot `ChatCallback`.

1. **Pre-flight gate**: if `non_system.len() ≤ keep_recent`, return `NoOp` without firing the LLM — even truncation couldn't shrink, so a `/compact` on a tiny conversation shouldn't burn tokens producing a single-line summary.
2. **Summarise**: send the full conversation + `SUMMARIZE_INSTRUCTION` through the `ChatCallback`. The whole transcript goes even though its tail is about to be kept verbatim, because `LlmCallInputs::Persisted` can only name the *entire* active set — trimming the request to a strict prefix would force an `Inline` marker and re-embed the transcript into every compaction span.
3. **Assemble** (`assemble_summary`): `[system…, summary, verbatim recent slice]`. The slice is a backward walk in atomic units (a message, or a `tool_use`/`tool_result` pair) bounded by `recent_slice_bounds(max_tokens)`, and it is what keeps a compaction from turning the last tool results and the user's own words into a paraphrase of themselves.
4. **Truncate fallback**: only when the summariser failed or returned nothing usable. Keep `system + last keep_recent non-system` messages, pair-preserving. This is the one path that makes no LLM call, so the agent loop records its `StepKind::Compression` step separately — otherwise the compaction that discarded the most would leave no trace at all.

Two things can make the flow decline instead:

- **The slice has to pay for itself.** It is re-added to the compacted transcript, so on a short conversation the walk can pull in nearly everything and the result comes out no smaller than its input. `assemble_summary` therefore tokenizes both `[system, summary, slice]` and `[system, summary]` and takes the first that is strictly smaller than the current count *and* at or below the ceiling whose crossing triggers the next compaction. No extra round-trip: the summary is already in hand. If neither fits, `run_compression`'s savings gate returns `NoSavings` and latches the transcript length — the threshold check runs at the top of every loop iteration with no backoff of its own, so without the latch the rest of the turn would be one full-transcript call per iteration. Growth past that length releases it; `force_compress` ignores and clears it.
- **A cancel is not a failure.** `/stop` mid-compaction surfaces as `ContextError::Cancelled` and returns `CompressOutput::Cancelled`, leaving the transcript untouched. Truncation is the only irreversible step in the flow, and the turn is unwinding — nothing further goes to the model, so destroying the middle of the conversation would be pure loss. A genuine transient failure is retried once (inside the same `Compression` step, so it reads as one compaction with two `LlmCall` spans) before the fallback; a non-retriable one — a context-window 400 is the likeliest — is not retried at all.

**Slice bounds.** `RECENT_SLICE_MAX_TOKENS_RATIO` (0.15) is window-relative, and **must stay below `compression_threshold`**: the tail rides along into the compacted transcript, so a ratio at or above the trigger would land every compaction back above its own threshold and re-fire it forever. `RECENT_SLICE_MAX_TOKENS_ABS` (40K) caps it on very large windows; the walk's token floor is expressed as a fraction of the derived cap so `min ≤ max` holds structurally rather than by coincidence at large sizes. On a window small enough that no tail fits, the compaction is summary-only.

The summary message follows Claude Code's continuation-prompt shape: an intro paragraph framing the conversation as resumed from compaction, the body prefixed with `Summary:`, a `read the full transcript at: <path>` pointer (resolved through `WorkspacePaths::session_log_file`) — a **virtual** path with no file behind it: a `Read` of it is served by a virtual-read resolver (`ReadTool` consults `ctx.virtual_reads` before the filesystem) from the durable `session_messages` transcript (full, including rows compaction has since superseded), and a closing paragraph instructing the model to resume work without acknowledging the summary. The footer has two variants because its claim has to be true — one says the recent messages are preserved verbatim below, one doesn't. `parse_summary_response` strips both `<analysis>` and `<summary>` tags.

Every `Replaced` return triggers `ContextManager` to insert the skill trailer right after the system block (`insert_skill_trailer`). The historical `<system-reminder>` carrying the skill list lives in a `User` message — the summary discards it by construction, and the truncate fallback can drop it whenever the reminder lands in the dropped middle. Re-inserting is cheaper than tracking whether the kept slice still carries one. The reminder block re-advertises the session's *filtered* set (`invocable_skill_summaries` — agent-invocable, non-untrusted, channel-admitted; skipped when empty), never the raw registry, so a hidden skill can't leak back in after compaction; the per-called-skill `<skill>` detail blocks stay keyed on `called_skills` unfiltered. Putting it adjacent to the system prompt also keeps the "what tools are available" context lined up for prompt caching.

### Context priority structure

The context sent to the LLM is organized in descending priority:

1. **System Prompt / Soul** — fixed, never compressed
2. **Compressed Summary** — elastic, grows as compression happens
3. **Recent Messages** — elastic, main recent history
4. **Current User Message** — fixed, always preserved

### Dependency boundaries

- Depends on `baybo-llm` for the `ChatRequest` / `LlmResponse` shape used in the `ChatCallback` signature. The compressor does not construct an LLM client itself; the callback is supplied by the caller. Tokenization stays algorithm-only: `TiktokenTokenizer` depends on `tiktoken-rs` (pure BPE), not on any provider SDK.
- Depends on `baybo-workspace` for `WorkspacePaths` so the transcript-recovery pointer resolves through the same source of truth the rest of the runtime uses.
- Does **not** depend on `memory` — memory recall is injected from the agent layer: `AgentLoop::recall_and_inject` recalls via `baybo-memory` and appends framed `RecalledMemory` rows through `ContextManager::append_recalled_memory`; context only supplies the envelope (`prompts/recalled_memory.rs`).
- Depends on `baybo-trace` only for the `LlmCallInputs` marker type carried through the `ChatCallback` — the compressor builds a `Persisted`-ordinal/`Inline` input marker for the span, but opening the span and recording cost still happen inside the caller's closure; `context` only sees its `Result<LlmResponse, ContextError>`. No direct `storage` dependency — transcript persistence is brokered through the `Arc<SessionManager>` (from `baybo-session`) supplied at construction.

## Constraints

- `TokenBudget::max_tokens` is sourced from the active LLM client's `ModelInfo::context_window` — installed by `AgentLoop::from_config` via `ContextManager::set_active_model_context_window`. There is no separate configured cap; resize the model's `context_window` if you need headroom for output tokens.
- `agent.context.compression_threshold` ships at `0.65` (`crates/config/src/agent.rs`). Raising it leaves less headroom for the compaction's own output; lowering it compacts more often.
- Tool-heavy conversations often need a larger `keep_recent`

## Cost recording

`ContextManager::maybe_compress` takes a chat closure from the caller and forwards it to the strategy as a `ChatCallback`. The agent loop's chat closure brackets the real LLM call in a `StepKind::Compression` step + `SpanKind::LlmCall` span (real lifecycle — start/end times, real `input_messages`) and calls `CostManager::record_call` with the span's id while the span is still open. The cost row's `span_id` is therefore a join key into a real trace span. `context` itself takes no `CostManager` dependency and never opens spans.

Failure handling: when the callback errors or returns empty content, the compressor falls back to a truncate slice (still returned as `CompressOutput::Replaced`). A summariser failure logs `warn!` and continues the user's turn rather than killing it.

## Token-count estimation: baseline + delta

`TiktokenTokenizer` is at best a ~10% approximation for non-OpenAI providers (Anthropic, Gemini, …) and under-counts because per-message estimates don't include the request envelope (system prompt, tools schema). The agent loop closes that gap by feeding the provider's authoritative `usage.input_tokens` back into `ContextManager` after every main LLM call as a **baseline**. Subsequent budget queries return `baseline.actual_tokens + tokenize(messages[count_at_call..])` — the bulk of the count is the provider's exact number, and the only thing we tokenize locally is the suffix appended since.

Lifecycle:

1. Cold start (no baseline) → `count_tokens` falls back to a full BPE-and-calibrate sweep.
2. After a main call lands `usage.input_tokens = N` for the current transcript of length `K` → `record_call_actual(N)` anchors the baseline (`actual_tokens=N`, `message_count_at_call=K`) and feeds a `(raw_estimate, N)` sample to `TokenCalibration` keyed by `current_model`. The slice argument from the previous API is gone — the manager owns the transcript outright now, so the call site has nothing useful to pass in.
3. Within the turn, each new assistant/tool message is appended; budget grows as `N + tokenize(suffix)`.
4. Compression mutates the prefix → `maybe_compress` calls `invalidate_baseline()`; next call resets the cycle.
5. Compression LLM calls are *not* fed into calibration — their input shape (old non-system messages, no tools schema) differs from main-call shape and would set a misleading baseline.

`TokenCalibration` (per-model EMA ratio of `actual / estimate`, α=0.3, samples clamped to [0.5, 2.0], estimates < 100 tokens skipped) is still applied to the **delta** part — single-message BPE error is small but non-zero. The full-sweep fallback path also goes through calibration so cold-start estimates are scaled too.

Wiring contract:

- `maybe_compress(model_id, chat)` is the single point that sets the calibration key. Pass `LlmCompletion::model_info().id` so `observe` and `adjust` key into the same bucket. Switching `model_id` between calls invalidates the baseline (the prior `actual_tokens` was tokenised by the old provider).
- `TiktokenTokenizer::for_model(model)` only picks the BPE family — it stores no model id. Calibration granularity is decided by what the agent loop passes into `maybe_compress`, not by how the tokenizer was constructed.
- Calibration state is in-memory only; cold start re-calibrates from scratch each process. The baseline is reset on every compression and re-anchored by the next main call.

## Collaboration

| Module   | Role                                                                                   |
| -------- | -------------------------------------------------------------------------------------- |
| `agent`   | `AgentLoop` owns a `ContextManager` instance and calls `append` / `maybe_compress`     |
| `session` | Required `Arc<SessionManager>` supplied to `ContextManager::from_config` (the `sessions` field of `ContextManagerConfig`); mirrors transcript mutations to `session_messages` |

## See also

There is no `CompressionStrategy` trait, no dispatch, and no swappable strategy type — one hardcoded flow, described above. `force_compress` (`/compact`) runs it without the budget gate.
