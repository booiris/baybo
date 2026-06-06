# context - Context Management

## Overview

The `context` crate owns the per-actor conversation state: the
transcript (`messages`), the token budget, and the hardcoded 3-stage
compression flow. Persistence is wired in directly — every
`ContextManager` takes a bound `SessionId` + `Arc<SessionManager>` at
construction; `append` and the compression apply mirror to
`session_messages` through the `SessionManager` wrapper in
[`aura-session`](session.md). Tests construct an in-memory store via
`aura_session::test_support::MemorySessionStore` and pass it through
the same constructor — no separate "in-memory mode" exists.

Core responsibilities:

- **Sole owner of the transcript**: `ContextManager` holds `Vec<ChatMessage>` directly. `Session` (in `aura-model`) carries only metadata (id, user, channel, lineage, soul binding, …). Every `append` calls `persist_appended` (→ `SessionManager::append_session_message`) and every successful compression calls `persist_compaction` (→ `SessionManager::apply_session_compaction`). Cold-start hydration via `restore_from_store` seeds the manager so an actor restart preserves the conversation.
- **Caller-driven compression**: `append()` is pure (push + budget update); the agent loop calls `maybe_compress()` at well-defined points so compression LLM cost can be recorded against the cost ledger
- **Token budget tracking**: track current token usage and remaining capacity via `TokenBudget`, anchored to the provider's authoritative `usage.input_tokens` between calls
- **Hardcoded compression flow**: a single `Compressor` impl block on `ContextManager` runs three stages in sequence — summary.md fast-path → live LLM summary → truncate fallback. No trait, no dispatch — every production session takes the same path.

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
├── workspace         — Arc<aura_workspace::WorkspacePaths>; resolves
│                       summary.md (fast-path), the transcript-recovery pointer,
│                       the identity files (soul assembly), and the
│                       tool-spills dir (oversize tool output)
├── system_prompt     — resolved system prompt for the initial seed (workspace
│                       soul or a subagent profile override); reseed-after-
│                       compaction re-reads the workspace instead
├── compressor.rs     — impl ContextManager block: 3-stage hardcoded flow
│   ├── Stage 1: try_summary_fast_path  — read summary.md, assemble
│   │                                     [system + summary + recent slice]
│   ├── pre-flight gate                 — NoOp if non_system.len() ≤ keep_recent
│   ├── Stage 2: LLM summary            — invoke ChatCallback with
│   │                                     SUMMARIZE_INSTRUCTION
│   ├── Stage 3: truncate fallback      — keep system + last keep_recent
│   │                                     (only on Stage 2 failure)
│   └── reseed_system_row               — re-read workspace soul on every apply
└── prompts/          — all model-facing framing text + pure builders
    ├── soul.rs        — assemble_from_workspace (TOP/TAIL hints + identity)
    ├── cron.rs        — frame_cron_prompt / original_cron_prompt
    ├── subagent.rs    — build_notification_content (SubagentNotification XML)
    ├── tool_output.rs — wrap_tool_output / cap_tool_output / spill (+ MAX cap)
    └── compression.rs — SUMMARIZE_INSTRUCTION + CONTINUATION_INTRO/FOOTER
```

`prompts/` is the single home for every piece of text the runtime injects into
the LLM transcript. The pure builders are unit-testable on their own; both
`ContextManager` (`resolve_system_prompt` via `ensure_seeded`,
`cap_tool_output`, `reseed_system_row`) and the agent-loop seam
(`append_cron_fire`, `append_subagent_notification`) call into them. The
injection *detection* for tool output stays in `aura-security`; only the
`<tool_output>` envelope formatting lives here (the shared delimiter is
`aura_model::TOOL_OUTPUT_{OPEN,CLOSE}_PREFIX`).

**Key design choice**: `ContextManager` is a **concrete struct** with a **concrete compression flow**. Both the management logic (append, budget check) and the compression algorithm are invariant — no swappable strategy, no extension trait. Per-session paths flow through one shared `WorkspacePaths` handle.

### Compression is caller-driven

`append()` only pushes the message and updates the token budget — it does **not** auto-compress. The agent loop calls `maybe_compress()` at the top of every iteration; that's the single point where compression LLM calls happen and where their cost is recorded against the cost ledger.

This trade-off — losing the "impossible to forget" property of auto-compression — is deliberate: every compression that reaches Stage 2 spawns a billable LLM call, and the cost-recording context (`SpanRecorder`, `JobId`, `CostManager`) only exists at the agent-loop layer. Auto-compressing inside `append()` would silently bypass that recording.

```rust
// Append in any number of places without cost-recording overhead.
self.context_manager.append(&user_msg).await;

// Single explicit compression site at the top of each iteration.
self.compress_if_needed(session, span_recorder, job_id, &cancel_token).await?;
```

`maybe_compress` returns `Result<CompressionOutcome>`, a four-variant enum: `Compressed` (the transcript was replaced with a shorter list), `BelowThreshold` (budget under the configured compression threshold; only produced by `maybe_compress`), `StrategyDeclined` (the compressor's pre-flight gate fired — non-system message count already at or below `keep_recent`, so even truncate couldn't shrink), or `NoSavings` (the compressor produced a candidate slice but its post-tokenise total wasn't smaller than the original). The chat closure supplied by the agent loop is what opens the `StepKind::Compression` step + `SpanKind::LlmCall` span and records the call against the cost ledger.

`force_compress` is the same call without the budget gate, for caller-initiated passes (e.g. a user-typed `/compact` slash command). The pre-flight NoOp and non-shrinking applies still surface as `StrategyDeclined` / `NoSavings`; only the threshold check is bypassed, so a too-small conversation is still left alone rather than rewritten as a one-line summary.

## Design Decisions

### Separation of concerns

| Concern                              | Owner                              | Rationale                                                                                  |
| ------------------------------------ | ---------------------------------- | ------------------------------------------------------------------------------------------ |
| Token budget (how much room is left) | `TokenBudget`                      | Pure state; agent can query `budget().remaining()` for other decisions                     |
| When to compress                     | `ContextManager::maybe_compress`   | Caller (agent loop) triggers at the top of each iteration so cost recording can be wrapped |
| How to compress                      | `compressor.rs` impl block         | Hardcoded 3-stage flow on `ContextManager`; no swappable strategy                          |
| Per-session paths                    | `Arc<WorkspacePaths>`              | Resolves `summary.md` for the fast-path and the transcript-recovery pointer for the message |
| Token counting                       | `Tokenizer` trait                  | Trait and `TiktokenTokenizer` impl both live here; no LLM-SDK coupling                     |
| Calibration key (which model)        | `maybe_compress`'s `model_id` arg  | Caller passes the LLM id at compression time; `ContextManager` stores and reuses it        |

### The 3-stage compression flow

`ContextManager::run_compression_flow` (in `compressor.rs`) is `async` and receives a one-shot `ChatCallback`. It runs the three stages in order:

1. **Stage 1 — `try_summary_fast_path`**: read `<state>/<session_id>/summary.md` via `WorkspacePaths::session_summary_file`, look up the summary's cursor in the persisted active log, and assemble `[system + summary blob + recent slice]`. Falls through on any of: no metadata, file missing, cursor stale, length mismatch, or assembled total > `0.6 × max_tokens`. Returns `Replaced { messages }` on success.
2. **Pre-flight gate**: if `non_system.len() ≤ keep_recent`, return `NoOp` without firing the LLM. Mirrors the old `Truncate` strategy's NoOp exit so a `/compact` on a tiny conversation doesn't burn tokens producing a single-line summary.
3. **Stage 2 — LLM summary** (`summarize_or_truncate`): send the full conversation + `SUMMARIZE_INSTRUCTION` to the model via the `ChatCallback`. On success, replace with `[system + parsed summary]` and return `Replaced { messages }`.
4. **Stage 3 — truncate fallback**: only reached when Stage 2 returns an error or empty content. Keep `system + last keep_recent non-system` messages (pair-preserving so tool_use / tool_result stays intact) and return `Replaced { messages }`.

The summary message itself follows Claude Code's continuation-prompt shape: an intro paragraph framing the conversation as resumed from compaction, the summary body (verbatim from `summary.md` for the fast-path; LLM output prefixed with `Summary:` for stage 2), a `read the full transcript at: <path>` pointer (resolved through `WorkspacePaths::session_log_file`) — a **virtual** path with no file behind it: a `Read` of it is served by a virtual-read resolver (`ReadTool` consults `ctx.virtual_reads` before the filesystem) from the durable `session_messages` transcript (full, including rows compaction has since superseded), and a closing paragraph instructing the model to resume work without acknowledging the summary. `parse_summary_response` strips both `<analysis>` and `<summary>` tags so the body lands cleanly in either path.

Every `Replaced` return triggers `ContextManager` to insert the skill trailer right after the system block (`insert_skill_trailer`). The historical `<system-reminder>` carrying the skill list lives in a `User` message — the summary stages discard it by construction, and the truncate fallback can drop it whenever the reminder lands in the dropped middle. Re-inserting unconditionally is cheaper than tracking whether the kept slice still carries one. Putting it adjacent to the system prompt also keeps the "what tools are available" context lined up for prompt caching.

### Context priority structure

The context sent to the LLM is organized in descending priority:

1. **System Prompt / Soul** — fixed, never compressed
2. **Compressed Summary** — elastic, grows as compression happens
3. **Recent Messages** — elastic, main recent history
4. **Current User Message** — fixed, always preserved

### Dependency boundaries

- Depends on `aura-llm` for the `ChatRequest` / `LlmResponse` shape used in the `ChatCallback` signature. The compressor does not construct an LLM client itself; the callback is supplied by the caller. Tokenization stays algorithm-only: `TiktokenTokenizer` depends on `tiktoken-rs` (pure BPE), not on any provider SDK.
- Depends on `aura-workspace` for `WorkspacePaths` so per-session paths (`summary.md`, transcript-recovery pointer) resolve through the same source of truth the rest of the runtime uses.
- Does **not** depend on `memory` (the agent loop has no automatic memory injection; the `memory` crate only powers the admin REST surface).
- Does **not** depend on `trace` or `storage` directly — the chat callback is what opens the trace span and records cost; `context` only sees its `Result<LlmResponse, ContextError>`. Persistence of the transcript is brokered through the `Arc<SessionManager>` (from `aura-session`) supplied at construction.

## Constraints

- `TokenBudget::max_tokens` is sourced from the active LLM client's `ModelInfo::context_window` — installed by `AgentLoop::from_config` via `ContextManager::set_active_model_context_window`. There is no separate configured cap; resize the model's `context_window` if you need headroom for output tokens.
- Compression threshold around 0.7–0.85 is usually reasonable
- Tool-heavy conversations often need a larger `keep_recent`

## Cost recording

`ContextManager::maybe_compress` takes a chat closure from the caller and forwards it to the strategy as a `ChatCallback`. The agent loop's chat closure brackets the real LLM call in a `StepKind::Compression` step + `SpanKind::LlmCall` span (real lifecycle — start/end times, real `input_messages`) and calls `CostManager::record_call` with the span's id while the span is still open. The cost row's `span_id` is therefore a join key into a real trace span. `context` itself takes no `CostManager` or trace dependency.

Failure handling: when `Summarize`'s callback errors or returns empty content, the strategy itself falls back to a Truncate-equivalent slice (still returned as `CompressOutput::Replaced`). A transient summarizer failure logs `warn!` and continues the user's turn rather than killing it.

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
| `session` | Required `Arc<SessionManager>` supplied to `ContextManager::new`; mirrors transcript mutations to `session_messages` |

## See also

- [`background-compression.md`](../background-compression.md) — async per-session summary maintenance. It feeds the summary fast-path **inside** the existing hardcoded `Compressor` (Stage 1 `try_summary_fast_path`, which reads the precomputed `summary.md` and swaps it in when fresh); it does **not** introduce a new strategy type. There is no `CompressionStrategy` trait, no dispatch, and no swappable `Summarize`/`SummaryAwareWrapper` — when the fast-path declines, the same `Compressor` falls through to the live LLM summary (Stage 2) and the truncate fallback (Stage 3). `force_compress` (`/compact`) runs that same flow without the budget gate.
