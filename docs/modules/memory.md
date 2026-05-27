# memory — pluggable long-term memory

## Overview

The `aura-memory` crate defines a **single pluggable [`Memory`] trait**. The
system knows memory only through one `Arc<dyn Memory>` slot (not a many-registry
like tools/channels): at most one implementation is registered at startup. The
trait is intentionally thin and **storage-opaque** — an implementation owns its
own persistence (libsql, a vector DB, an external service) and receives its LLM
+ embedding handles and config in its own constructor.

The core ships the trait, its value types (`MemoryContext`, `RecalledMemory`,
`MemoryError`), and a **`NoopMemory`** reference default. **No real backend ships
yet** — the runtime wires `None` (the inert no-op path), so in production today
nothing is recalled, written, or billed. The plumbing below activates the moment
a real `Arc<dyn Memory>` is registered.

This supersedes the previous CRUD `MemoryManager` facade (now removed, along with
`MemoryStore`/`MemoryEntry`/`MemoryCategory`, the libsql impl, the `/v1/memory`
admin REST, and the `memories` table — dropped by migration). The earlier
heuristic `recall` + `maybe_store` pipeline was retired because it (1) recalled
by arbitrary substring match, (2) treated entire assistant outputs as memorable,
and (3) re-injected snapshots as `Role::System` messages that polluted every
later turn. Those three failure modes are now **hard constraints** (below), not a
reason to forbid the whole shape.

## The trait

```rust
// crates/memory/src/lib.rs
pub struct RecalledMemory { pub content: String }

// Carries the real (user, session, job) + the trace recorder + the enclosing
// memory step. `scoped_llm_call(begin, body)` opens an LlmCall span under that
// step and hands `body` an Attribution bound to it, for billed sub-calls.
pub struct MemoryContext { /* user_id, session_id, job_id, recorder, step */ }

#[async_trait]
pub trait Memory: Send + Sync {
    async fn recall(&self, ctx: &MemoryContext, query: &[ContentBlock])
        -> Result<Vec<RecalledMemory>>;
    async fn on_job_complete(&self, ctx: &MemoryContext,
        user_input: &[ContentBlock], final_output: &[ContentBlock]) -> Result<()>;
    async fn on_session_end(&self, ctx: &MemoryContext,
        transcript: &[ChatMessage]) -> Result<()>;
    fn tools(&self) -> Vec<(Arc<dyn Tool>, ToolManifest)> { Vec::new() }
}
```

- **`recall`** — synchronous query, on the critical path (job start + each
  interjection). De-duplication against already-surfaced memories is **internal
  to the impl** (keyed off `ctx`; one impl is a process singleton, so that state
  survives actor reap/rehydration for free). The core injects exactly what is
  returned.
- **`on_job_complete` / `on_session_end`** — fire-and-forget lifecycle events
  (the read/write asymmetry is intentional; the verb is **not** "sync", which
  would imply bidirectional reconciliation). `on_job_complete` sees one finished
  exchange (`user_input` includes mid-turn interjections); `on_session_end` sees
  the full durable transcript at idle-timeout.
- **`tools`** — the model's "explicit signal" path, coexisting with the
  automatic recall/write path. Registered statically at startup.

## Clients & billing

The implementation holds the **unbound** `Arc<BillableLlm>` and a billed
embedding handle, constructor-injected. The core hands each call a
`MemoryContext` carrying the real `(user, session, job)` + the trace recorder +
the enclosing `MemoryRecall` / `MemoryWrite` step; the impl binds its handles per
billed sub-call via `MemoryContext::scoped_llm_call` (below), so spend bills to
the **real** user/session under a real span (mirrors `compression.rs`, not
`Attribution::system`).

**Span attribution.** `cost_records.span_id` is written by `record_call` keyed
off `attribution.span_id`, so a billed sub-call needs a *real* span or its cost
row is orphaned. `MemoryContext::scoped_llm_call(begin, body)` provides one: it
opens an `LlmCall` span **under the memory step**, hands `body` an `Attribution`
bound to that span (built from the real user/session/job + the span id), and
closes the span with the call's token usage. The impl binds its `BillableLlm` /
billed embedding handle with that attribution and makes the call — so the cost
row lands on a recorded span, attributed to the real user/session/job. Embedding
calls record as `LlmCall` spans too (a model call is a model call for cost/trace
purposes). The impl never constructs a bare `Attribution`, so it can't bill
against an orphaned id.

A new **`EmbeddingClient`** trait lives in `aura-llm` beside `BilledChat`:
batch `embed(&[String]) -> EmbeddingResponse` + `dimensions()`, sealed behind
`BillableEmbedding` / `BoundBilledEmbedding` so embedding spend flows through the
identical micro-USD guard→record chokepoint as chat. Trait + billed wrapper only;
no concrete provider ships yet.

## Recall injection (enforces hard constraint #3)

Recalled memories enter the prompt as a **persisted, framed** block — never
`Role::System`. The path mirrors the user-interjection pattern:

- `MessageSource::RecalledMemory` (`aura-model`) + `ChatMessage::recalled_memory`.
- `aura_context::prompts::recalled_memory` — a `<recalled_memory>` envelope,
  re-derived wire-only in `ContextManager::messages_for_llm` (via
  `frame_recalled_memories`, alongside `frame_interjections` — both delegate to
  one `frame_source_runs` helper). The budget counts the framed size.
- Persisted once per recall (`append_recalled_memory`); rides the transcript
  until compression folds it. Hidden from the chat bubble surface
  (`from_user() == false`), so it never renders as a user turn.
- The core does **no** dedup — it injects exactly what `recall` returns.

## Flow & hook points (scope: `UserChat` + `Cron` jobs only)

The agent loop (`agent_loop.rs`) drives memory; `System`, `Spawned` (subagent),
and `SubagentNotification` jobs are excluded (no direct user input → would
pollute / double-write). `memory_recall_query(&JobInput)` is the gate — an
exhaustive match that returns the query for `UserChat`/`Cron` and `None`
otherwise (forces classification when a `JobInput` variant is added).

1. **Recall — inline, job start.** After the user message is in context and
   before the first LLM call, `recall_and_inject` opens a `MemoryRecall` step,
   mints `Attribution`, calls `recall`, and injects each result as a
   `RecalledMemory` row. Recall failure is logged and swallowed — never fails
   the turn.
2. **Recall — inline, interjection.** Each mid-turn interjection drained at a
   tool boundary is recalled against too, and folded into the job's input.
3. **`on_job_complete` — background, job end.** At `IterationOutcome::Final`,
   `spawn_job_complete_write` detaches a task (so the actor returns the answer
   without waiting) that opens a `MemoryWrite` step and calls `on_job_complete`.
   **Only on a clean `Final`** — a max-iterations, cancelled, or errored turn
   writes nothing (those paths return before this point), so memory only ever
   sees completed exchanges.
4. **`on_session_end` — interface only; caller deferred.** The trait method
   exists and `NoopMemory` implements it, but the idle-timeout **trigger is not
   wired yet** (see Deferred). A real backend currently relies on the per-job
   write for durability.

With `memory == None` every hook above is skipped entirely — no trace step is
opened and nothing is billed, so the no-op path is genuinely inert.

## Config

`MemoryConfig` on `AuraConfig` (`crates/config/src/memory.rs`): typed
core-wiring knobs (`enabled`, `llm` entry name, `embedding_provider`,
`embedding_model`) **plus** an opaque `extra: serde_json::Value` passed through
verbatim to the plug-in. The `extra` bag is a deliberate, documented exception to
the "typed over `Value`" rule — plug-in config is genuinely opaque to the core.
Memory config is **not** hot-reloadable (`reload.rs` classifies it non-hot).

## Hard constraints (carried forward from the retired pipeline)

1. **No substring recall.** Recall is embedding/LLM-judged relevance, never
   substring match against free-form text.
2. **No whole-output storage.** The write path must judge salience, never treat
   an entire assistant output as a memory.
3. **No `Role::System` re-injection.** Recalled memories use the persisted,
   wire-framed `RecalledMemory` path only — the core enforces this structurally
   (it is the single injection path).

These are the implementation's contract.

## Deferred

- **`on_session_end` caller wiring.** Triggering whole-session consolidation at
  idle-timeout means emitting from / extending the supervisor's `reap_idle`
  (which today only sends `ActorStop` and holds no `SpanRecorder`). It is inert
  under the no-op default (production behaviour is identical whether or not it is
  wired), so it was left as a follow-up to keep this change focused. The trait
  method + `NoopMemory` impl are in place; `on_job_complete` already captures
  incremental facts.
- **Concrete embedding provider + the first real `Memory` impl.** Out of scope —
  the trait + billed wrapper + all wiring are ready for one to drop in at the
  single construction point in `runtime.rs::build_managers`.
- Operator/GDPR wipe of memory rows: re-add behind a user-triggered command if a
  future backend needs it (no background sweeper — see CLAUDE.md).

## Collaboration

| Module      | Role                                                                            |
| ----------- | ------------------------------------------------------------------------------- |
| `model`     | `MessageSource::RecalledMemory`, `ChatMessage::recalled_memory`                 |
| `llm`       | `Attribution`; `EmbeddingClient` + `BillableEmbedding`/`BoundBilledEmbedding`   |
| `tools`     | `Tool` / `ToolManifest` for `Memory::tools()`                                   |
| `context`   | `<recalled_memory>` framing + `append_recalled_memory` + budget                 |
| `agent`     | Drives `recall` / `on_job_complete`; `AgentLoopConfig.memory`                   |
| `config`    | `MemoryConfig` (typed knobs + opaque `extra`)                                   |
| `trace`     | `StepKind::MemoryRecall` / `MemoryWrite`                                         |
