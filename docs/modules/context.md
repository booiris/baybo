# context - Context Management

## Overview

The `context` crate owns the per-actor conversation state: the
transcript (`messages`), the token budget, and the compression
strategy. Pure in-memory — persistence is the agent loop's job,
brokered through `SessionStore` from `storage` (the wrapper lives
in [`aura-session`](session.md)).

Core responsibilities:

- **Sole owner of the transcript**: `ContextManager` holds `Vec<ChatMessage>` directly. `Session` (in `aura-model`) carries only metadata (id, user, channel, lineage, soul binding, …). The agent loop persists each appended message and each compaction through `SessionStore`'s `append_session_message` / `apply_session_compaction`; cold-start hydration via `load_active_session_messages` seeds `ContextManager` so an actor restart preserves the conversation.
- **Caller-driven compression**: `append()` is pure (push + budget update); the agent loop calls `maybe_compress()` at well-defined points so compression LLM cost can be recorded against the cost ledger
- **Token budget tracking**: track current token usage and remaining capacity via `TokenBudget`, anchored to the provider's authoritative `usage.input_tokens` between calls
- **Pluggable compression strategies**: swap compression algorithms without changing management logic

**Goal**: ensure the context sent to the LLM never exceeds the model's context window while preserving the most valuable information.

## Architecture

```
ContextManager (struct)
├── TokenBudget       — pure state: max_tokens, threshold, current usage
├── Tokenizer         — trait, counts tokens (no model id — calibration
│                       is keyed by the model_id passed into maybe_compress)
│   └── TiktokenTokenizer — BPE impl via `tiktoken-rs` (cl100k_base / o200k_base)
├── TokenCalibration  — optional, per-model EMA ratio of actual/estimate
│                       fed back from `AgentLoop::call_llm`
├── current_model     — Option<String>; written by maybe_compress, used as
│                       calibration key + baseline-invalidation trigger
└── CompressionStrategy — trait, the only extension point
    ├── Truncate          — keep system + last N messages
    └── Summarize         — truncate + LLM summarization
```

**Key design choice**: `ContextManager` is a **concrete struct**, not a trait. The management logic (append, budget check) is invariant — only the compression algorithm varies. Polymorphism lives at the `CompressionStrategy` trait level, not at the manager level.

### Compression is caller-driven

`append()` only pushes the message and updates the token budget — it does **not** auto-compress. The agent loop calls `maybe_compress()` at the top of every iteration; that's the single point where compression LLM calls happen and where their cost is recorded against the cost ledger.

This trade-off — losing the "impossible to forget" property of auto-compression — is deliberate: with `Summarize` as the default strategy every compression spawns a billable LLM call, and the cost-recording context (`SpanRecorder`, `JobId`, `CostManager`) only exists at the agent-loop layer. Auto-compressing inside `append()` would silently bypass that recording.

```rust
// Append in any number of places without cost-recording overhead.
self.context_manager.append(session, &user_msg);

// Single explicit compression site at the top of each iteration.
self.compress_if_needed(session, span_recorder, job_id, &cancel_token).await?;
```

`maybe_compress` returns `Result<CompressionOutcome>`, a three-variant enum: `NoOp` (under threshold or strategy couldn't shorten), `NoLlmCall` (compression ran without an LLM call — `Truncate`, or `Summarize` falling back after a callback failure), or `LlmCompressed(CompressionLlmCall)` (LLM-backed compression — the agent loop opens a `StepKind::Compression` step + `SpanKind::LlmCall` span and records the call against the cost ledger). `CompressionLlmCall` is purely an in-process handoff — never serialized.

`force_compress` is the same call without the budget gate, for caller-initiated passes (e.g. a user-typed `/compact` slash command). Strategy NoOp / non-shrinking applies still surface as `NoChange`; only the threshold check is bypassed, so a too-small conversation is still left alone rather than rewritten as a one-line summary.

## Design Decisions

### Separation of concerns

| Concern                              | Owner                              | Rationale                                                                                  |
| ------------------------------------ | ---------------------------------- | ------------------------------------------------------------------------------------------ |
| Token budget (how much room is left) | `TokenBudget`                      | Pure state; agent can query `budget().remaining()` for other decisions                     |
| When to compress                     | `ContextManager::maybe_compress`   | Caller (agent loop) triggers at the top of each iteration so cost recording can be wrapped |
| How to compress                      | `CompressionStrategy`              | Only variation point; swapped via constructor injection                                    |
| Token counting                       | `Tokenizer` trait                  | Trait and `TiktokenTokenizer` impl both live here; no LLM-SDK coupling                     |
| Calibration key (which model)        | `maybe_compress`'s `model_id` arg  | Caller passes the LLM id at compression time; `ContextManager` stores and reuses it        |

### Two compression strategies

Both strategies are **pure planners**: `compress` is sync, returns a [`CompressOutput`], and never performs I/O. The agent loop fulfils the plan (and brackets any LLM call in a real trace span / cost row).

- **Summarize** (default in production): condenses old non-system messages into a single `[Conversation Summary]` block via an LLM call and keeps the most recent `keep_recent` non-system messages alongside it. Returns `CompressOutput::NeedsLlmCall` carrying the prepared `ChatRequest`, an `on_success` closure that assembles `[system, summary-as-system, recent...]` from the response text, and an `on_failure` truncate-equivalent slice the caller falls back to when the LLM call errors.
- **Truncate**: keeps only the most recent N non-system messages plus all system messages. Returns `CompressOutput::Replaced(messages)` directly. Simple, zero latency, predictable — but discards early context. Used as the explicit choice in test harnesses where deterministic behavior matters more than semantic preservation, and as the shape of `Summarize`'s `on_failure` fallback.

### Context priority structure

The context sent to the LLM is organized in descending priority:

1. **System Prompt / Soul** — fixed, never compressed
2. **Memory Context** — fixed, injected by `agent`
3. **Compressed Summary** — elastic, grows as compression happens
4. **Recent Messages** — elastic, main recent history
5. **Current User Message** — fixed, always preserved

### Dependency boundaries

- Depends on `aura-llm` for the `ChatRequest` shape used in `CompressOutput::NeedsLlmCall` and for the chat closure's response type. Strategies do not call any LLM client themselves; the closure is supplied by the caller. Tokenization stays algorithm-only: `TiktokenTokenizer` depends on `tiktoken-rs` (pure BPE), not on any provider SDK.
- Does **not** depend on `memory` (memory context injected by `agent`).
- Does **not** depend on `trace` or `storage` — the chat closure is what opens the trace span and records cost; `context` only knows about the closure's `Result<String, ContextError>`.

## Constraints

- Keep `max_tokens` slightly below the real model limit to reserve output space
- Compression threshold around 0.7–0.85 is usually reasonable
- Tool-heavy conversations often need a larger `keep_recent`

## Cost recording

Strategies are pure planners: they return a [`CompressOutput`] describing the work, never making LLM calls themselves. `ContextManager::maybe_compress` takes a chat closure from the caller and invokes it only when the strategy returns `NeedsLlmCall`. The agent loop's chat closure brackets the real LLM call in a `StepKind::Compression` step + `SpanKind::LlmCall` span (real lifecycle — start/end times, real `input_messages`) and calls `CostManager::record_call` with the span's id while the span is still open. The cost row's `span_id` is therefore a join key into a real trace span. `context` itself takes no `CostManager` or trace dependency.

Failure handling: if the chat closure errors, `maybe_compress` falls back to the strategy's deterministic `on_failure` slice (typically a Truncate-equivalent — `system_msgs + recent`). A transient summarizer failure logs `warn!` and continues the user's turn rather than killing it.

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

- `maybe_compress(session, model_id)` is the single point that sets the calibration key. Pass `LlmCompletion::model_info().id` so `observe` and `adjust` key into the same bucket. Switching `model_id` between calls invalidates the baseline (the prior `actual_tokens` was tokenised by the old provider).
- `TiktokenTokenizer::for_model(model)` only picks the BPE family — it stores no model id. Calibration granularity is decided by what the agent loop passes into `maybe_compress`, not by how the tokenizer was constructed.
- Calibration state is in-memory only; cold start re-calibrates from scratch each process. The baseline is reset on every compression and re-anchored by the next main call.

## Collaboration

| Module   | Role                                                                                   |
| -------- | -------------------------------------------------------------------------------------- |
| `agent`  | `AgentLoop` owns a `ContextManager` instance and calls `append` / `maybe_compress`     |
| `memory` | Memory context is injected into the context window by `agent`, not by `context` itself |
