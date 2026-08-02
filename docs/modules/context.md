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
- **Hardcoded compaction flow**: a single impl block on `ContextManager` — one blocking summariser call, assembled with the verbatim tail. No trait, no dispatch — every production session takes the same path. **A summary is the only thing that shortens a transcript**: when the call fails, nothing is applied and the user is told.

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
├── system_prompt_version — Option<SystemPromptVersion>; the paths + `stat` of
│                       the files behind `messages[0]`, as of the last time
│                       `reconcile_system_prompt` agreed the two matched.
│                       `None` = unknown (cold start, or a compaction just
│                       rewrote the transcript) ⇒ look properly
├── compressor.rs     — impl ContextManager block: the compaction flow
│   ├── pre-flight gate    — NoOp if the conversation is short by BOTH
│   │                        measures (≤ keep_recent msgs AND under
│   │                        MIN_COMPACTABLE_TOKENS)
│   ├── summarize          — invoke ChatCallback with SUMMARIZE_INSTRUCTION;
│   │                        Failed (nothing applied) if it errors or the
│   │                        answer carries no summary
│   ├── assemble_summary   — [system + summary + verbatim recent slice],
│   │                        or summary-only when the slice would not shrink
│   └── reseed_system_row  — re-read workspace soul on every apply, BEFORE
│                            persist_compaction so the refresh is durable
└── prompts/          — all model-facing framing text + pure builders
    ├── soul.rs            — assemble (TOP/TAIL hints + identity) → AssembledPrompt
    ├── system_prompt_update.rs — build_blocks / wrap_update
    │                            (<system_prompt_update> drift delta,
    │                             per-section unified diff)
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
`cap_tool_output`, `reseed_system_row`, `reconcile_system_prompt`) and the agent-loop seam
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

This trade-off — losing the "impossible to forget" property of auto-compression — is deliberate: every compression that reaches Stage 2 spawns a billable LLM call, and the cost-recording context (`SpanRecorder`, `TurnId`, `CostManager`) only exists at the agent-loop layer. Auto-compressing inside `append()` would silently bypass that recording.

```rust
// Append in any number of places without cost-recording overhead.
self.context_manager.append(&user_msg).await;

// Single explicit compression site at the top of each iteration.
self.compress_if_needed(session, span_recorder, turn_id, &cancel_token, delta_tx.as_ref(), &mut compaction_failure_reported).await?;
```

`maybe_compress` returns `Result<CompressionOutcome>`: `Compressed` (the transcript was replaced with a shorter list), `BelowThreshold` (budget under the configured compression threshold; only produced by `maybe_compress`), `StrategyDeclined` (the compressor's pre-flight gate fired — the conversation is short by both measures, so a summary could not come out smaller), `NoSavings` (the compressor produced a candidate slice but its post-tokenise total wasn't smaller than the original), `Cancelled` (a `/stop` aborted the summariser call), or `Failed { reason }` (the summariser call errored, or answered with nothing usable). The chat closure supplied by the agent loop is what opens the `StepKind::Compression` step + `SpanKind::LlmCall` span and records the call against the cost ledger.

`Failed` is the one outcome the **user** is told about: the agent loop emits a `Warn` notice (once per turn — the gate re-fires on every remaining iteration) and `/compact` answers with a warning instead of a confirmation. The `reason` has already passed the leak boundary inside `CompressionRunner`, so it is safe to show.

`force_compress` is the same call without the budget gate, for caller-initiated passes (e.g. a user-typed `/compact` slash command). The pre-flight NoOp and non-shrinking applies still surface as `StrategyDeclined` / `NoSavings`; only the threshold check is bypassed, so a too-small conversation is still left alone rather than rewritten as a one-line summary.

### The system prompt's lifecycle

`messages[0]` is written once, at `ensure_seeded`, and **persisted**. A session
outlives the deploy that seeded it, so the row is a snapshot of whatever the
persona files and the binary's framing text said that day. Three seams keep it
honest, in ascending order of cost:

1. **`reconcile_system_prompt` — before every main LLM call.** Compares a
   `SystemPromptVersion` (the resolved source paths plus a `stat` of each) with
   the version recorded when the row was written. Equal ⇒ return; that is the
   hot path, and it reads no file. Different ⇒ re-assemble, diff the assembly's
   parts against what `messages[0]` says, and append the parts it does not — as
   a `MessageSource::SystemPromptUpdate` row (hidden from chat surfaces, framed
   `<system_prompt_update>`).

   The delta is **appended, never written over `messages[0]`**: that row heads
   the prompt-cache prefix, and rewriting it mid-session discards the provider's
   cached copy of the entire transcript to correct a few lines. A moved
   `personas/` directory costs one `<soul …>` block at the tail instead.

   Each update is a **complete delta against the leading row**, not against the
   update before it, so the newest fully restates every older one and the framing
   tells the model the last one wins. Superseded updates are deliberately **not**
   filtered out of the request: dropping a row from the middle of the transcript
   rewrites the request at that position and invalidates the cached prefix after
   it — the same cost as rewriting `messages[0]`, paid repeatedly. Appending at
   the tail is the only edit a cached prefix tolerates. What bounds the size
   instead is keeping each delta to the parts that moved, plus two filters: a
   version that moved without changing any bytes the prompt carries (a re-save, a
   `touch`) appends nothing, and neither does a delta the newest update already
   states verbatim.

   **A changed section is sent as a line diff, not as its new body.** Sections
   are addressed by tag, so `seeded_section_body` recovers the copy `messages[0]`
   carries and `similar` diffs it against the live one: a session that appended
   one line to `MEMORY.md` sends `<memory path="…" diff>` with that line and two
   lines of context, not the whole index. Two cheaper cases fall out of the same
   comparison — bytes identical, path moved, is `<tag path="…" content_unchanged/>`
   with no body at all, and a hunk-free diff never happens. Full text stays the
   fallback for everything a diff cannot beat: a hint (no tag, so no prior copy to
   address), a section `messages[0]` never carried, and a wholesale rewrite, whose
   diff quotes both copies and is measured against the body before it is chosen.
   Each explanatory paragraph is likewise carried only by an update that needs it,
   so the common single-section delta explains neither diffs nor self-edits.

   **A file the model rewrote itself is named, not restated.** `Edit`/`Write`
   calls in the transcript name the paths this conversation changed
   (`self_edited_source_paths`, keyed off `baybo_tools::FILE_WRITING_TOOLS`), and
   a section whose file is among them degrades to
   `<tag path="…" edited_by_you_in_this_conversation/>` plus one extra framing
   paragraph. Echoing an 11 KB `personas/USER.md` back at the model to tell it
   what it just wrote is the one case where the body is pure duplication. The
   scoping matters in both directions: a hint has no file behind it and is never
   elided, a change made by anyone else still arrives in full, and a path that
   fails to match (a symlink, a `..` spelling) falls back to the full body. And
   because the pointer is a pure function of tag and path, a second edit to the
   same file produces an identical update that the dedupe drops — repeated
   self-edits cost one row, not one per edit.

   The reconciler is otherwise append-only, which leaves one gap it has to close
   explicitly: a source that moves and then moves **back** leaves the standing
   update asserting something no longer true, and an empty delta cannot be
   silence. `wrap_update(&[])` renders a retraction instead, and only when an
   update is actually standing.

2. **`reseed_system_row` — after a committed compaction.** Replaces `messages[0]`
   with a fresh assembly. Runs **after** the savings gate (so a grown soul cannot
   veto a real compaction) and **before** `persist_compaction` (so the refreshed
   row is what the store keeps). That ordering is the whole of the durability:
   with the reseed after the persist, the store held the pre-reseed prompt
   forever and a reaped-then-rehydrated actor read it straight back — the
   compaction retired the stale prompt only for as long as the actor happened to
   live. Once the reseed lands, the reconciler goes quiet on its own: the
   assembly it compares against is exactly what `messages[0]` now holds.

   The compaction also **drops every `SystemPromptUpdate` row**, in `summarize`'s
   partition — before the tail walk, not after the assembly. The reseed rewrites
   `messages[0]` from the same assembly those updates were derived from, so
   keeping one would leave a block telling the model that a now-current prompt is
   out of date; and since the walk runs backward from the tail, which is exactly
   where updates are appended, filtering late would let each one consume the
   verbatim slice's token budget on its way to being discarded.

   The summariser *request* still carries them: it is built from `self.messages`
   whole, because `LlmCallInputs::Persisted` can only name the entire active set
   and trimming to a strict prefix would force an `Inline` marker that re-embeds
   the transcript into every compaction span. Accepted residue, not an oversight.

3. **`ensure_seeded` — fresh session.** Resolves and appends, then records the
   version it wrote — but **only when the resolve succeeded**. A seed that fell
   back to `FALLBACK_SYSTEM_PROMPT` records nothing, because stamping a version
   on the one-liner would claim it is the assembly of those files and mute the
   reconciler for the life of the session: a transient `EIO` while seeding would
   cost that conversation its whole persona, permanently and silently. Left
   `None`, the next call re-resolves and appends what the seed could not read.
   Idempotent; the leading-`System` check short-circuits ahead of the resolve, so
   only a fresh session pays.

`system_prompt_version` is set to `None` on hydration (`restore_messages`) and
on a compaction apply — in both cases the rows came from somewhere this actor
cannot vouch for, and `None` means "look properly". A subagent session's version
is its profile name: the registry is in-memory and immutable for the process, so
nothing can move under it mid-session, and a profile edited across a deploy is
caught by the hydration `None` like any other.

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

1. **Pre-flight gate**: return `NoOp` without firing the LLM when `non_system.len() ≤ keep_recent` **and** the non-system total is under `MIN_COMPACTABLE_TOKENS` — both, not either. The count alone says nothing about the summariser, which collapses any number of messages into one: gating on it refused to compact a transcript of a few pasted files — ten messages, 26k tokens, well past the budget — for as many turns as it stayed under the count. The token half is what the gate is really for: a `/compact` on a genuinely tiny conversation declines rather than burning a call to produce something longer than what it replaced.
2. **Summarise**: send the full conversation + `SUMMARIZE_INSTRUCTION` through the `ChatCallback`. The whole transcript goes even though its tail is about to be kept verbatim, because `LlmCallInputs::Persisted` can only name the *entire* active set — trimming the request to a strict prefix would force an `Inline` marker and re-embed the transcript into every compaction span.
3. **Assemble** (`assemble_summary`): `[system…, summary, verbatim recent slice]`. The slice is a backward walk in atomic units (a message, or a `tool_use`/`tool_result` pair) bounded by `recent_slice_bounds(max_tokens)`, and it is what keeps a compaction from turning the last tool results and the user's own words into a paraphrase of themselves.
4. **No fallback**: if the summariser errored or returned nothing usable, return `Failed { reason }` and leave the transcript exactly as it was. There is deliberately no "drop the middle" path. It existed once, and a two-second provider blip (`Our servers are currently overloaded`, both attempts) is all it took to cut a 1000-message conversation down to its last handful — silently, since a truncate reported as a successful compaction. A transcript that is still over threshold is a far smaller problem than one that lost its history: the next turn retries, and the user is told meanwhile.

Two things can make the flow decline instead:

- **The slice has to pay for itself.** It is re-added to the compacted transcript, so on a short conversation the walk can pull in nearly everything and the result comes out no smaller than its input. `assemble_summary` therefore tokenizes both `[system, summary, slice]` and `[system, summary]` and takes the first that is strictly smaller than the current count *and* at or below the ceiling whose crossing triggers the next compaction. No extra round-trip: the summary is already in hand. If neither fits, `run_compression`'s savings gate returns `NoSavings` and latches the transcript length — the threshold check runs at the top of every loop iteration with no backoff of its own, so without the latch the rest of the turn would be one full-transcript call per iteration. Growth past that length releases it; `force_compress` ignores and clears it.
- **A `/stop` aborts the compaction; the next turn redoes it.** The summariser call is raced against the turn's cancel token, so a cancel is answered promptly instead of waiting out the read timeout (600s). The abandoned call still costs its tokens, but the transcript is left exactly as it was, and it is still over budget, so the threshold check at the top of the next turn runs the compaction again. `ContextError::Cancelled` stays distinct from `ContextError::Compression` so a cancel is *not* reported to the user as a failure: nothing went wrong. A transient failure (as opposed to a cancel) is retried once inside the same `Compression` step, so it reads as one compaction with two `LlmCall` spans; a non-retriable one — a context-window 400 is the likeliest — is not retried at all. Either way the step closes `Failed`, carrying the reason.

**Slice bounds.** `RECENT_SLICE_MAX_TOKENS_RATIO` (0.15) is window-relative, and **must stay below `compression_threshold`**: the tail rides along into the compacted transcript, so a ratio at or above the trigger would land every compaction back above its own threshold and re-fire it forever. `RECENT_SLICE_MAX_TOKENS_ABS` (40K) caps it on very large windows; the walk's token floor is expressed as a fraction of the derived cap so `min ≤ max` holds structurally rather than by coincidence at large sizes. On a window small enough that no tail fits, the compaction is summary-only.

The summary message follows Claude Code's continuation-prompt shape: an intro paragraph framing the conversation as resumed from compaction, the body prefixed with `Summary:`, a `read the full transcript at: <path>` pointer (resolved through `WorkspacePaths::session_log_file`) — a **virtual** path with no file behind it: a `Read` of it is served by a virtual-read resolver (`ReadTool` consults `ctx.virtual_reads` before the filesystem) from the durable `session_messages` transcript (full, including rows compaction has since superseded), and a closing paragraph instructing the model to resume work without acknowledging the summary. The footer has two variants because its claim has to be true — one says the recent messages are preserved verbatim below, one doesn't. `parse_summary_response` strips both `<analysis>` and `<summary>` tags.

Every `Replaced` return triggers `ContextManager` to insert the skill trailer right after the system block (`insert_skill_trailer`). The historical `<system-reminder>` carrying the skill list lives in a `User` message, which the summary discards by construction. Re-inserting is cheaper than tracking whether the kept slice still carries one. The reminder block re-advertises the session's *filtered* set (`invocable_skill_summaries` — agent-invocable, non-untrusted, channel-admitted; skipped when empty), never the raw registry, so a hidden skill can't leak back in after compaction; the per-called-skill `<skill>` detail blocks stay keyed on `called_skills` unfiltered. Putting it adjacent to the system prompt also keeps the "what tools are available" context lined up for prompt caching.

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
- `agent.context.keep_recent` is the message-count half of the pre-flight gate — how many non-system messages still count as a *short* conversation. It no longer sizes a kept tail: the verbatim slice after a summary is sized in tokens by `recent_slice_bounds`.

## Cost recording

`ContextManager::maybe_compress` takes a chat closure from the caller and forwards it to the strategy as a `ChatCallback`. The agent loop's chat closure brackets the real LLM call in a `StepKind::Compression` step + `SpanKind::LlmCall` span (real lifecycle — start/end times, real `input_messages`) and calls `CostManager::record_call` with the span's id while the span is still open. The cost row's `span_id` is therefore a join key into a real trace span. `context` itself takes no `CostManager` dependency and never opens spans.

Failure handling: when the callback errors or returns nothing usable, the compressor returns `CompressOutput::Failed { reason }` and applies nothing. The step closes `Failed`, the loop logs `warn!` and emits a `Warn` notice to the user, and the turn continues rather than dying — the transcript is simply still over threshold, and the next pass retries.

## Token-count estimation: baseline + delta

`TiktokenTokenizer` is at best a ~10% approximation for non-OpenAI providers (Anthropic, Gemini, …) and under-counts because per-message estimates don't include the request envelope (system prompt, tools schema). The agent loop closes that gap by feeding the provider's authoritative `usage.input_tokens` back into `ContextManager` after every main LLM call as a **baseline**. Subsequent budget queries return `baseline.actual_tokens + tokenize(messages[count_at_call..])` — the bulk of the count is the provider's exact number, and the only thing we tokenize locally is the suffix appended since.

Lifecycle:

1. Cold start (no baseline) → `count_tokens` falls back to a full BPE-and-calibrate sweep.
2. After a main call lands `usage.input_tokens = N` for the current transcript of length `K` → `record_call_actual(N)` anchors the baseline (`actual_tokens=N`, `message_count_at_call=K`) and feeds a `(raw_text_estimate, N)` sample to `TokenCalibration` keyed by `current_model`. The slice argument from the previous API is gone — the manager owns the transcript outright now, so the call site has nothing useful to pass in.
3. Within the turn, each new assistant/tool message is appended; budget grows as `N + tokenize(suffix)`.
4. Compression mutates the prefix → `maybe_compress` calls `invalidate_baseline()`; next call resets the cycle.
5. Compression LLM calls are *not* fed into calibration — their input shape (old non-system messages, no tools schema) differs from main-call shape and would set a misleading baseline.

`TokenCalibration` (per-model EMA ratio of `actual / estimate`, α=0.3, samples clamped to [0.5, 2.0], estimates < 100 tokens skipped) is still applied to the **delta** part — single-message BPE error is small but non-zero. The full-sweep fallback path also goes through calibration so cold-start estimates are scaled too.

### Media is priced outside the calibration loop

`Tokenizer::count_message_media` splits a message's estimate in two, and `per_message_tokens` stores both halves (`MessageTokens { text, media }`):

- **text** — what the BPE really counted. Calibration scales this and only this.
- **media** — what the provider will bill for an image, a native PDF, a voice note or an inlined document, priced by `baybo_llm::content_block_tokens` from facts probed or stat'd at ingest (`width`/`height`, `page_count`, `duration_ms`, `size_bytes`) and falling back to that arm's delivery cap when a fact is absent. Added to the budget beside the calibrated text, never through it.

  Charged **only on rows the provider actually receives media on**, which `baybo_llm::delivers_media(role)` decides: `build_completion_request` runs `user_content_for_block` — the only path that materialises bytes — on a **user** row alone. An assistant row keeps text, tool calls and thinking; a system row is flattened to text; a tool row keeps only its result. Media anywhere else is dropped without even a stub.

  The predicate lives in `baybo-llm`, beside the conversion that implements it, precisely because re-deriving it in this crate is what let the price and the delivery decision drift apart: `content_block_tokens` takes a bare `&ContentBlock` and never sees a role, so the budget charged assistant-row media in full while the wire dropped it. `delivers_media_matches_the_conversion_for_every_role` pins the two together, and the drift is asymmetric — over-charging only compacts early, while under-charging produces an over-window request the provider rejects on every turn.

  The live case is the agent loop folding `AttachFile` output onto the turn's final **assistant** row so the file persists and rebuilds on cold start, not so the model re-reads it. Uncorrected, one attached SVG — no pixel grid to read, so it prices at the full `IMAGE_TOKEN_CEILING` of 9,288 — burned that much window for as long as the row survived, against a provider charge of zero.

Both directions matter. As the *denominator* of a sample, a ceiling makes every observation over-count, so the EMA walks the ratio to the `SAMPLE_RATIO_MIN = 0.5` clamp — measured on a realistic session (5,000 tokens of chat + one `.md`; raw 22,529, provider actual 5,500) the ratio hit 0.5288 by turn 8, and from there a 40,002-token plain-CJK transcript on the *same model* was charged 21,154. As something the ratio *multiplies*, a ceiling stops being a ceiling.

So a sample is refused **when the ceiling is big enough to swamp the signal** — `media > MAX_MEDIA_SHARE_FOR_SAMPLE (0.25) × raw_total`. That share bounds an admitted sample's inflation at a third **only while the media charge is an over-estimate of what the provider bills**, which is what the delivery caps in `baybo-llm` guarantee: every arm prices from a fact probed at ingest, and delivery re-derives the same fact from the same bytes and stubs anything its cap cannot cover. Break that premise and the gate reads the under-estimate on both sides at once — a 12000×9000 image charged 9,288 against a provider 49,536 sat at exactly 25% of a 27,864-token transcript, was admitted, and every sample read 2.78, clamped to `SAMPLE_RATIO_MAX` and walked the EMA there. `TokenCalibration` is one process-wide instance cloned into every `ContextManager`, so unrelated sessions on that model then charged plain text at 2× and compacted early. There is deliberately **no** second clamp on how far one sample may move the ratio: `SAMPLE_RATIO_MAX` already caps the sample and α caps the step, the walk came from repeated samples out of one contaminated session rather than from an outlier, and a tighter step would only slow convergence on the 1.4–1.5× drift the loop exists to track. Refusing every media-bearing transcript instead is not the neutral choice it looks like: one image in message 1 then suppresses the sample for the life of the session and the whole text history falls back to the identity ratio. Measured at 1.5× provider drift over eight turns with a 40k-token tool result landing after the last call — text only: ratio 1.471, budget 208,915 vs provider 210,068 (−0.5%); plus one image, sampled: 1.514, 216,832 vs 216,266 (+0.3%); plus one image, refused: identity, 196,263 vs 216,266 (−9.2%), and that miss grows with whatever lands after the anchor. A PDF at the delivery cap (93,600) is ~70% of the same transcript and is refused. The baseline is still anchored every turn, so the uncalibrated part of a refused session is only the suffix appended since the last call.

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
