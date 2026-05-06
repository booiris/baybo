# context - Context Management

## Overview

The `context` crate manages Aura's session context window, a central component inside Agent Loop.

Core responsibilities:

- **Context appending with auto-compression**: append messages and automatically compress when the token budget threshold is exceeded
- **Token budget tracking**: track current token usage and remaining capacity via `TokenBudget`
- **Pluggable compression strategies**: swap compression algorithms without changing management logic

**Goal**: ensure the context sent to the LLM never exceeds the model's context window while preserving the most valuable information.

## Architecture

```
ContextManager (struct)
├── TokenBudget       — pure state: max_tokens, threshold, current usage
├── Tokenizer         — trait, counts tokens
│   └── TiktokenTokenizer — BPE impl via `tiktoken-rs` (cl100k_base / o200k_base)
└── CompressionStrategy — trait, the only extension point
    ├── Truncate          — keep system + last N messages
    └── Summarize             — truncate + LLM summarization
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

`maybe_compress` returns `Option<CompressStats>` — `None` if no compression occurred, `Some(stats)` if it did. The `CompressStats::llm_call` field carries the `CompressionLlmCall` provenance the agent loop needs to record cost.

## Design Decisions

### Separation of concerns

| Concern                              | Owner                    | Rationale                                                              |
| ------------------------------------ | ------------------------ | ---------------------------------------------------------------------- |
| Token budget (how much room is left) | `TokenBudget`            | Pure state; agent can query `budget().remaining()` for other decisions |
| When to compress                     | `ContextManager::append` | Auto-triggered on threshold; impossible to forget                      |
| How to compress                      | `CompressionStrategy`    | Only variation point; swapped via constructor injection                |
| Token counting                       | `Tokenizer` trait        | Trait and `TiktokenTokenizer` impl both live here; no LLM-SDK coupling |

### Two compression strategies

- **Summarize** (default in production): combines truncation with LLM summarization. Old non-system messages are summarized via an injected `SummarizeCallback`, then the summary is kept alongside the most recent `keep_recent` messages. The `SummarizeCallback` trait keeps `context` independent from `llm` — the callback is injected externally; the production implementation is `aura_agent::compression::LlmSummarizer`. On any callback failure (transport error, rate limit, empty content), `Summarize` logs a `warn!` and falls back internally to a Truncate-equivalent slice — a single transient summarizer failure must never kill the user's turn.
- **Truncate**: keep only the most recent N non-system messages, always preserving system messages. Simple, zero latency, predictable — but discards early context. Used as the internal fallback inside `Summarize::compress` and as the explicit choice in test harnesses where deterministic behavior matters more than semantic preservation.

### Context priority structure

The context sent to the LLM is organized in descending priority:

1. **System Prompt / Soul** — fixed, never compressed
2. **Memory Context** — fixed, injected by `agent`
3. **Compressed Summary** — elastic, grows as compression happens
4. **Recent Messages** — elastic, main recent history
5. **Current User Message** — fixed, always preserved

### Dependency boundaries

- Does **not** depend on `llm` (SummarizeCallback trait defined locally; `TiktokenTokenizer` depends only on `tiktoken-rs`, a pure BPE algorithm crate — not an LLM provider SDK)
- Does **not** depend on `memory` (memory context injected by `agent`)
- Does **not** depend on `trace`

## Constraints

- Keep `max_tokens` slightly below the real model limit to reserve output space
- Compression threshold around 0.7–0.85 is usually reasonable
- Tool-heavy conversations often need a larger `keep_recent`

## Cost recording

When `maybe_compress` returns `Some(stats)` with `stats.llm_call`, the agent loop's `compress_if_needed` opens a `StepKind::Compression` step containing one `SpanKind::LlmCall` span (post-hoc — the LLM call already ran inside the strategy), and inside that span's lifecycle calls `CostManager::record_call` with the matching `span_id`. The cost row's `span_id` therefore joins back to the trace span; downstream UIs can navigate cost → trace by id without extra plumbing. Cost recording lives entirely on the agent side; `context` itself takes no `CostManager` dependency.

## Collaboration

| Module   | Role                                                                                   |
| -------- | -------------------------------------------------------------------------------------- |
| `agent`  | `AgentLoop` owns a `ContextManager` instance and calls `append` / `maybe_compress`     |
| `memory` | Memory context is injected into the context window by `agent`, not by `context` itself |
