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

### Auto-compression in `append`

Compression is triggered automatically inside `append()` when the token budget threshold is exceeded. The caller does not need to (and cannot) manually trigger compression. This eliminates the class of bugs where compression is forgotten after tool results or other message appends.

```rust
// Agent loop — simple, no manual compression step
self.context_manager.append(session, &user_msg).await?;
// If compression happened, it's already done and logged.
```

`append` returns `Option<CompressStats>` — `None` if no compression occurred, `Some(stats)` if it did.

## Design Decisions

### Separation of concerns

| Concern                              | Owner                    | Rationale                                                              |
| ------------------------------------ | ------------------------ | ---------------------------------------------------------------------- |
| Token budget (how much room is left) | `TokenBudget`            | Pure state; agent can query `budget().remaining()` for other decisions |
| When to compress                     | `ContextManager::append` | Auto-triggered on threshold; impossible to forget                      |
| How to compress                      | `CompressionStrategy`    | Only variation point; swapped via constructor injection                |
| Token counting                       | `Tokenizer` trait        | Trait and `TiktokenTokenizer` impl both live here; no LLM-SDK coupling |

### Two compression strategies

- **Truncate**: keep only the most recent N non-system messages, always preserving system messages. Simple, zero latency, predictable — but may discard important early context.
- **Summarize**: combines truncation with LLM summarization. Old non-system messages are summarized via an injected `SummarizeCallback`, then the summary is kept alongside the most recent `keep_recent` messages. The `SummarizeCallback` trait keeps `context` independent from `llm` — the callback is injected externally.

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

## TODO

- **Concrete `SummarizeCallback` implementation**: The `Summarize` strategy is implemented in `context`, but it requires a `SummarizeCallback` to be injected at construction. A concrete implementation that wraps `LlmClient::chat()` needs to be built in the `agent` crate to bridge the two. Until then, only `Truncate` is usable end-to-end.

## Collaboration

| Module   | Role                                                                                   |
| -------- | -------------------------------------------------------------------------------------- |
| `agent`  | `AgentLoop` owns a `ContextManager` instance and calls `append` / `maybe_compress`     |
| `memory` | Memory context is injected into the context window by `agent`, not by `context` itself |
