# Remove background compression — implementation plan

**Status:** implemented on `refactor/remove-background-compression`; the operator runbook in §8
has NOT been run. Kept as the record of why each decision was made — the surviving design lives in
[`docs/modules/context.md`](../modules/context.md). One PR, spanning
`baybo-context` / `baybo-agent` / `baybo-session` / `baybo-store` / `baybo-storage` /
`baybo-workspace` / `baybo-trace` / `baybo-model` / `baybo-query`, `app/web`, `app/ios`, and docs.

Deletes the detached background-summary pass and the compressor's `summary.md` fast path. What
survives is the blocking inline compaction: the token threshold trips at the top of an agent-loop
iteration, the turn blocks on one live summarization call, and the transcript is rewritten before
the next LLM call. `/compact` (`force_compress`) runs the same flow without the threshold gate.

`docs/background-compression.md` is deleted by this work; the surviving definition moves to
`docs/modules/context.md` §"The compaction flow".

---

## 1. Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | Delete the background pass **and** stage 1 outright — `summary.md`, the `session_summaries` table, `SessionSummaryStore`, `BackgroundCompressionRunner`, `run_background_summary`, `reap_orphan_summaries`, the anchor/cursor pair, `try_summary_fast_path`. | The anchor-vs-cursor state machine, the unproductive-pass backoff, and the orphan reaper are the bulk of the complexity; keeping the notes file on the blocking path would put an agentic ≤10-round Read/Edit loop in front of the user. |
| D2 | Stage 2 keeps a **verbatim recent slice**: `[system…, summary, recent slice…]`, not `[system…, summary]`. | Stage 1 was the only compaction that kept verbatim recent context. Without this, every compaction turns the last tool results and the user's own last words into third-person prose. `walk_backward_atomic` / `pair_preserving_cut` / the `RECENT_SLICE_*` consts therefore **survive**. |
| D3 | Delete `CompressionTrigger::Background`, `CompressionApplied::StoredSummary`, `CompressionStage::StoredSummary`, `CompressionTrigger::changes_next_input`, and the TS mirrors. | Nothing produces them after D1. |
| D4 | Historical trace rows are fixed by a **manual one-off SQL scrub** run before deploy. No boot-time migration. The `Option<>` wrappers on `trigger`/`applied` **stay**. | Repo rule: no legacy-data cleanup migrations. The `Option`s are load-bearing for pre-#222 rows, which omit the keys entirely (`serde(default)` covers a missing key, not an unknown value). |
| D5 | **`/stop` aborts an in-flight compaction, and the next turn redoes it.** The summariser call is raced against the cancel token; the abandoned call costs its tokens but the transcript is left untouched, still over budget, so the next turn's threshold check compacts it. A real LLM *failure* (not a cancel) retries once and only then falls back to truncate. No user-facing notice. | Answering `/stop` promptly matters more than salvaging the in-flight call — without the race the turn cannot unwind until the summariser returns, up to the 600s read timeout. Truncate is the only irreversible step in the flow, so a cancel must not reach it: nothing was learned about the transcript. Pinned by `a_stop_aborts_the_compaction_and_the_next_turn_redoes_it`, verified to fail when a cancel is treated as a summariser failure. |
| D6 | iOS gets the compaction indicator (a `status` frame → a work-block step). Web chat unchanged. No new persisted control event. | iOS drops `Frame::Status` entirely today, so a 30–90s blocking compaction is indistinguishable from a hung turn. The durable trace of a compaction is already the `compaction_points` divider; the status line only has to explain the stall while it lasts. |
| D7 | `RECENT_SLICE_MAX_TOKENS` becomes relative: `min(40_000, 0.15 × max_tokens)`; the token floor scales with it. `compression_threshold` stays `0.65`. | The absolute 40K was safe only because stage 1 sat behind a `0.6 × max_tokens` fall-through gate. Stage 2 is the terminal stage — nothing to fall through to — so on a 32K window a 40K slice makes every compaction land *above* its own trigger threshold. |
| D8 | The summarizer still receives the **full** transcript; `SUMMARIZE_INSTRUCTION` gains a static paragraph saying recent messages may be preserved verbatim below the summary. No message count substituted in. | `LlmCallInputs::Persisted{last_ordinal, prefix_len, suffix}` can only express "the whole active set + a suffix". Sending a strict prefix forces `Inline`, re-embedding the transcript into every compaction span — the O(n²) trace-disk regression PR #179 fixed. |
| D9 | Stage 2 picks between two candidate assemblies with the tokenizer, reusing the summary it already has: `[system+summary+slice]`, else `[system+summary]`, else let `NoSavings` fire. No extra LLM call. | Without this, `/compact` on a mid-sized conversation burns a full-transcript call and then refuses to apply, because the slice makes the candidate larger than the original. |
| D10 | One PR: Rust + `app/web` + `app/ios` + docs. | Owner's call. Deploy ordering still separates the artifacts — see §7. |
| D11 | Two hardening items ship in the same PR: (a) the trace-read `collect::<Result<_,_>>()?` sites degrade one row instead of a whole job; (b) `web_trace_types_sync.rs` also pins the two compression unions. | (a) is the only protection if a deployment's DB misses the scrub. (b) guards exactly the enums this PR edits. |
| D12 | Test scope: the three coverage gaps left by the deletions, plus one guard per new behaviour (D2, D5, D6, D7, D9, A9). | See §5. |
| D13 | Add an in-memory `compaction_declined_at_len` latch. | D2 disarms the pre-flight `non_system.len() <= keep_recent → NoOp` gate that stops re-compaction today, and `compress_if_needed` runs once per loop iteration with no backoff on `NoSavings`. |
| D14 | Fix the `message_fts` duplicate-hit bug in this PR. | `apply_session_compaction` indexes its re-inserted rows and `search_messages` has no `compaction_inserted` predicate, so a kept message returns two hits. Pre-existing (stage 1 already re-inserted its slice) and not amplified by this change, but it is one predicate away. |

## 2. Amendments found by verification

Each of these is a decision above that the code does not support as stated.

**A1 — D5's cancel branch was unreachable.** `compression.rs` did a bare `bound.chat(&request).await`;
the token was used only to build `cancel_ctx` for trace classification. So the pre-existing behaviour
of a `/stop` mid-compaction was to wait out `LLM_READ_TIMEOUT` (600s, `crates/llm/src/error.rs:126`)
and *then* truncate. Fixed by racing the call:
`tokio::select! { biased; _ = cancel.cancelled() => …, res = bound.chat(request) => res }`,
mirroring `agent_loop.rs:1689-1696`.

**A2 — cancellation needs a type.** `LlmError` is flattened to `String` twice on the way out, so the
compressor could not tell a cancel from a failure. `ContextError::Cancelled(String)` →
`CompressOutput::Cancelled` → `CompressionOutcome::Cancelled`, and the compressor returns it instead
of falling through to the truncate fallback.

**A3 — the retry belongs in `CompressionRunner`, not the compressor.** `ChatCallback` is `FnOnce`
and `run(self, …)` consumes the runner. Implement it as a two-iteration loop around `with_llm_span`
**inside** the single `with_step`: one `Compression` step, two `LlmCall` spans. Classify with
`baybo_llm::LlmError::is_retriable` **before** the `anyhow` conversion at `compression.rs:188` —
`BadRequest` (a context-window 400 is the likeliest compaction failure), `Auth`, and `GuardRejected`
must not consume the retry. Cancel never retries. This is the compaction path's *first* retry layer,
not a second: `ErrorHandler` wraps `call_llm` only (`docs/modules/agent.md:206-214`), never compaction.

**A4 — `Compacted` is emitted unconditionally** (`agent_loop.rs`). Keep it unconditional except when
the token is tripped: the compaction was abandoned and the turn is unwinding behind it, so there is
no line left to close. Otherwise the end edge means "the pass finished", not "the transcript
changed" — it fires on a truncate fallback and a no-savings decline too.

**A5 — a cancelled `/compact` would print a lie.** `NoOp` maps to `StrategyDeclined`, which prints
"nothing to summarize (conversation too short)". `CompressionOutcome::Cancelled` gets its own
neutral string; the match is exhaustive, so this is compiler-forced.

**A6 — pin D9's predicate.** `before_tokens = budget.current()` is provider-anchored;
`after_tokens = calibrate(Σ message_budget_tokens)` (`lib.rs:1038,1048-1058`). A loose predicate can
pick a candidate the outer gate then rejects. Use:

```
cand_tokens(msgs) = calibrate(Σ message_budget_tokens(m))
                  + estimate_skill_trailer_tokens(registry, tokenizer, &self.called_skills,
                                                  &self.invocable_skill_summaries())
accept(c)         = cand_tokens(c) < budget.current()
                 && cand_tokens(c) <= (max_tokens as f64 * compression_threshold) as usize
```

Both helpers are private on `ContextManager` and `compressor` is a child module — no visibility
change needed. `TokenBudget::threshold` has no getter; add `pub fn threshold(&self) -> f64`. Use the
same `message_budget_tokens` closure inside the backward walk (not raw `tokenizer.count_message`) so
the walk and the gate agree.

**A7 — the skill trailer is inserted after the compressor returns** (`lib.rs:1029-1035`) and is
absolute-capped (`PER_SKILL_TOKEN_CAP` 5K / `TOTAL_SKILL_TOKEN_CAP` 25K), not window-relative.
A6's `cand_tokens` must include the trailer estimate, computed from `&self.called_skills` — what
`lib.rs:1033` actually inserts — not from `scan_skill_calls(&slice)`.

**A8 — D7 over-specifies.** `RECENT_SLICE_MIN_TEXT_BLOCK_MSGS` is a message count, and the cap check
`break`s first and unconditionally (`compressor.rs:195-208`), so it can never exceed the cap. Scale
only the token floor, expressed as a fraction of the derived cap so `min <= max` is structural:

```rust
pub(crate) const RECENT_SLICE_MAX_TOKENS_ABS: usize = 40_000;
/// MUST stay below `compression_threshold` — if it ever inverts, every threshold
/// compaction lands above its own trigger and `NoSavings` becomes permanent.
pub(crate) const RECENT_SLICE_MAX_TOKENS_RATIO: f64 = 0.15;
pub(crate) const RECENT_SLICE_MIN_RATIO_OF_CAP: f64 = 0.25;
pub(crate) const RECENT_SLICE_MIN_TEXT_BLOCK_MSGS: usize = 5;

fn recent_slice_bounds(max_tokens: usize) -> (usize, usize, usize) {
    let cap = RECENT_SLICE_MAX_TOKENS_ABS
        .min((max_tokens as f64 * RECENT_SLICE_MAX_TOKENS_RATIO) as usize);
    (
        (cap as f64 * RECENT_SLICE_MIN_RATIO_OF_CAP) as usize,
        RECENT_SLICE_MIN_TEXT_BLOCK_MSGS,
        cap,
    )
}
```

**A9 — D13's latch, precisely.** `compaction_declined_at_len: Option<usize>` on `ContextManager`;
set to `self.messages.len()` when `run_compression` returns `NoSavings`; cleared by any append that
grows past it; `maybe_compress` short-circuits to `NoSavings` with **no LLM call** while
`self.messages.len() <= latch`. `force_compress` ignores and clears it.

**A10 — D8's wording and D9's candidate B contradict each other**, and `CONTINUATION_FOOTER`
(`prompts/compression.rs:13`) already claims "Recent messages are preserved verbatim." — false for
stage 2 today. (a) Word the new paragraph as *may be preserved*, and never license omission: the
verbatim copy, when present, is a supplement, not a substitute. (b) Place it in the prose block after
"…without losing context." and before "Before providing your final summary…" — never inside the
numbered sections or the tool-refusal fence. (c) Split `CONTINUATION_FOOTER` into with-slice /
without-slice variants, chosen after the D9 pick.

**A11 — `SummaryChatRun` is defined in the file D1 deletes** (`background_summary.rs:150`) but is the
inline path's return type. Delete it; `CompressionRunner::run` returns `Result<LlmResponse, ContextError>`
and both call sites drop `.map(|run| run.response)`.

**A12 — a third enum was missed.** `baybo_context::CompressionStage::StoredSummary`
(`compressor.rs:85-93`, matched at `agent_loop.rs:2596-2602`). `record_spanless_compaction`'s match
collapses to `LiveSummary => return, Truncate => CompressionApplied::Truncate`.

**A13 — an unscrubbed row wedges boot recovery, not just the trace page.** `close_job_subtree`
collects with `?` (`recovery.rs:339-344`), so one undecodable row leaves that job non-terminal and
re-fails on every boot. The scrub is therefore a **mandatory pre-deploy step**, and D11a's treatment
extends to `recovery.rs:343`.

**A14 — the contract test is half-blind.** `declared_tags` truncates the union body at the first
`";\n"` (`web_trace_types_sync.rs:144`); #222's multi-line `compression` member contains
`kind: 'compression';`, so the scrape currently sees **2 of 7** `StepKind` tags. Fix the terminator to
`";\n\n"` **before** adding coverage. The two compression unions are flat string unions with no
`kind: '` needle — they need a second scraper, not a parameter. Also apply D11a to the span collect
(`query/src/lib.rs:552`) and the span-event collect (`:519`); `:466` is in `load_job`, which has zero
callers — the 500 that matters comes from `:542` (`load_step_tree_for_job`).

---

## 3. Rust edit order

Leaf-first; the workspace compiles after each commit under
`cargo clippy --all --benches --tests --examples --all-features -- -D warnings`.

**C1 — trace-read hardening (D11a, A13b, A14).** Land first, independently reviewable.
`query/src/lib.rs:466,519,542,552` and `recovery.rs:343` → `filter_map` + `warn!` carrying the row id.
Note where dropping a row orphans children (a span whose step vanished) and log accordingly.

**C2 — test deletions.** Delete `integration-tests/tests/background_compression_e2e.rs` and
`summary_aware_wrapper_e2e.rs` plus their `all.rs` `#[path] mod` lines (`autotests = false`; a stale
`mod` is a hard error). Salvage first: `any_request_invoked_summarizer`, the `Arc<AtomicUsize>`
call-counting idiom, and `fresh_store_and_paths()`.

**C3 — agent layer.** `agent_loop.rs`: delete `maybe_run_background_compression` (`:2906-3031`), both
call sites (`:1054-1063`, `:1088-1097`), the `bg_compression*` fields + initialisers, the
`workspace_paths` field and its `AgentLoopConfig` entry, and `mod bg_compression_at_most_one_tests`
(`:3669-3730`). Modify `:2596-2602` (A12), `:2550-2568` (A4), `:2881-2894` (A5), `:2562`/`:2869` (A11).
`runtime/compression.rs`: delete `BackgroundCompressionRunner` and `reap_orphan_summaries`, rewrite the
module doc, and apply A1/A2/A3/A11 to `CompressionRunner::run`. `baybo/src/runtime.rs`: delete the
`reap_orphan_summaries` boot call (`:447-452`) and the `workspace_paths:` field (`:897`) — keep the
`workspace_paths_arc` binding, `ContextManagerConfig.workspace` still needs it.
**Survives despite appearances:** `AgentLoop::sessions` (3 readers), `record_spanless_compaction`
(the truncate arm is the only trace a truncate compaction gets), `recover_detached_trace_rows`
(title generation and the progress observer also outlive their job).

**C4 — context crate, one commit** (splitting orphans `walk_backward_atomic` into a `dead_code`
failure). Delete `background_summary.rs`, the `SUMMARY_TRIGGER_*` / `SUMMARY_DIFF_*` consts and
`summary_diff_threshold`, `FAST_PATH_FALLTHROUGH_THRESHOLD_RATIO`, the anchor/cursor fields and their
five methods, the `fast_path_summary_index` threading, the `repoint_summary_cursor` block, and
`restore_from_store` step 2 (**step 1 — the crash-torn `repair_tool_pairing` heal — survives**).
Delete `try_summary_fast_path`; rewrite `summarize_or_truncate` for D2/D7/D9 + A2/A6/A7/A10c. Add the
A8 bounds fn, the A9 latch, `TokenBudget::threshold()`, `ContextError::Cancelled`,
`CompressionOutcome::Cancelled`. Drop `baybo-tools` and `tokio-util` from `Cargo.toml` (they existed
for the background tool loop).
**Survives despite appearances:** `pair_preserving_cut` (the truncate fallback's only guard against
splitting a `tool_use`/`tool_result` pair — a provider 400 on the failure path of the failure path),
`partition_system`, `parse_summary_response`, `insert_skill_trailer`, `scan_skill_calls` (also used by
`crates/cli/src/commands/session.rs`), `synced_last_ordinal`, `ContextManagerConfig.workspace`.

**C5 — session → store → storage, one commit** (the `SessionManager::new` arity change ripples to 24
call sites). Delete the `summary_store` field + param, the five summary methods,
`load_active_session_messages_up_to` (trait + impls + fake), `store/src/session_summary.rs`,
`storage/src/sqlite/session_summary.rs`, `MemorySessionSummaryStore`, the `session_summary` bundle
field, and the `session_summaries` CREATE in `init_db` (`mod.rs:685-718`) — **no `DROP`, no
`ADD_COLUMNS` entry**; existing rows stay orphaned, per the `job_transitions` precedent at `:796-801`.
Delete `query/src/lib.rs:2462-2560` (`replay_matches_write_side_load_up_to_across_compaction`) — its
premise no longer exists. Apply D14 here: `search.rs` `search_messages` gains
`AND m.compaction_inserted = 0`.
**Survives despite appearances:** `active_index_of_ordinal` (the **background-notification** delivery
ledger uses it — different feature, colliding name), `count_active_messages` and
`latest_session_ordinal` (the surviving compaction's own `LlmCallInputs::Persisted` marker),
`apply_session_compaction`, and everything named `SessionSummary*` in `baybo-query` (that is the admin
trace-browser session list).

**C6 — workspace.** Delete `STATE_SESSIONS_SUBDIR`, `SUMMARY_FILE`, `SUMMARY_FILE_TMP`,
`state_sessions_dir`, `session_state_dir`, `session_summary_file`, `session_summary_tmp_file`, their
assertions, and the `ensure_layout` entry that mints the dir at boot.
**Do not let an `rg 'sessions'` sweep eat** `SESSIONS_LOG_SUBDIR` / `sessions_log_dir` /
`session_log_file` / `sanitize_session_id` — that is the virtual-transcript family, still used by the
surviving summary message and by `virtual_read.rs`.

**C7 — model + trace + sync test.** Delete `BackgroundCompressionPayload`. Drop the two enum variants
and `changes_next_input`, rewrite their docs. Apply A14 to `web_trace_types_sync.rs` and add
`web_trace_types_cover_the_compression_unions` (both directions, with `match` exhaustiveness tripwires
and a negative control so the helper cannot go vacuous).

## 4. `app/web`

`src/types/trace.ts` — drop `| 'background'` and `'stored_summary' |`, rewrite both doc blocks, delete
`changesNextInput`. `src/api/mock.ts:350,372` — retarget to `'threshold'` / `'live_summary'`, drop the
`bgLlm` span that exists only to illustrate the background pass, rewrite the narrative comments.
`src/components/trace/traceFormat.tsx:278-281` — prose only ("a stored-summary or truncate compaction
makes no LLM call" → truncate only); `stepSummaryText` is a value passthrough, no logic change.
`traceFormat.test.ts:71,81` — retarget the two fixtures, keeping `:86`'s `forced · truncate` case
distinct. `ChatPage.tsx:6011` — the divider tooltip claims the model sees only a summary of what is
above the line; with D2 the rows just above it are still in context. **No change** to `STEP_VISUALS`,
`TRACE_LEGEND`, `TraceTree.tsx`, or the chat status handling.

## 5. `app/ios` — four files move in lockstep

`web/src/wire.test.ts:46` asserts an **exact set** equality between `WIRE_KINDS` and a regex scrape of
`case "…":` in `Transcript.tsx`, so a partial edit fails.

- `web/src/types.ts` — add `| { kind: "status"; phase: string }` to `WireFrame`.
- `web/src/wireSentinel.ts` — add `"status"` to `MirroredKind`.
- `web/src/wire.test.ts` — add `"status"` to `WIRE_KINDS`.
- `web/src/Transcript.tsx` — `case "status":` after `tool_completed`: `foldStreamingIntoProse()`,
  `pushWorkStep({ kind: "status", text: compactionStatusText(t, frame.phase) })`, and `runSync()` on
  `"compacted"` (iOS's equivalent of web's `refreshCompactionPoints` — without it the divider appears
  up to 3 minutes late).
- `web/src/WorkBlock.tsx` — add `compactionStatusText(t, phase)`. **Must be a ternary chain, not a
  `switch`, and must not live in `Transcript.tsx`** — the wire test's regex would scrape its `case`
  labels.
- `web/src/locales/{en,zh}.ts` — `"Compacting context…"` / `"Context compacted"`, `"正在压缩上下文…"` /
  `"上下文已压缩"` (U+2026).
- `web/src/Transcript.tsx:174-179` — rewrite the `AWAITING_MAX_MS` doc; its "never expires under a live
  turn" claim is already false. **Do not raise the number** — it is the only automatic escape from a
  hard composer lockout. The status work step keeps `workLive` true, which keeps `running` true, which
  is what actually stops the backstop from flipping the stop button back to send mid-compaction.
- `Transcript.tsx:277-278` — same divider-copy fix as web.
- **No Swift and no Rust FFI change**: `ChatStore.pushFrame` forwards frames verbatim with no
  allowlist, and the transport has no per-kind lane to add.

## 6. Tests

**Delete** — the two e2e files; `mod summary_diff_threshold_tests`; the three fast-path/cursor tests;
the 8-test anchor block; `mod bg_compression_at_most_one_tests`; the `background_summary.rs` and
`sqlite/session_summary.rs` in-crate tests; `query/src/lib.rs:2462-2560`.

**Keep untouched** — all 13 `pair_preserving_cut` / `walk_backward_atomic` tests (D2 keeps them live);
the `parse_summary_response` tests; `context_compression_e2e.rs`'s two tests (extend, do not replace);
the skill-trailer tests; `traceFormat.test.ts:85-97`; `workStep.test.tsx:57-64`.
**Verify early:** `agent_loop_e2e.rs:1842 retry_reanchors_prompt_after_compaction_supersedes_it` asserts
`active_prompt_rows == 1`; D2's slice can make it 2. It is the cheapest signal that the slice is doing
something unexpected to the durable active set.

**Add** — gaps left by the deletions: a spanless `Truncate` compaction step (zero coverage today),
"a summarizer failure does not kill the turn", and the `NoSavings` `/compact` string. Then one guard
per new behaviour:

- **D2** — `stage2_keeps_a_verbatim_recent_slice_after_the_summary`;
  `stage2_slice_never_splits_a_tool_use_result_pair`; `stage2_slice_retains_a_skill_call_in_called_skills`;
  `stage2_persists_the_kept_slice_as_compaction_inserted_rows` (real sqlite store).
- **D5** — `cancelled_compaction_leaves_the_transcript_untouched` (blocking-LLM fixture + a real
  `UserStopped` cancel; asserts the transcript is byte-identical, no notice, no trailing `Compacted`,
  under a 5s timeout so a regression fails fast instead of hanging);
  `compaction_llm_failure_retries_once_then_truncates` (one `Compression` step, **two** `LlmCall` spans);
  `compaction_llm_failure_that_succeeds_on_retry_applies_the_summary`;
  `non_retriable_compaction_error_does_not_retry`;
  `empty_summary_response_falls_back_without_a_second_call`.
- **D6** — `wire.test.ts`'s exact-set assertion becomes the routing guard for free; add a
  `compactionStatusText` mapping test including the raw-phase fallback.
- **D7** — `recent_slice_cap_scales_with_the_window`; `recent_slice_minima_never_exceed_the_cap` across
  `[0, 1_000, 8_192, 50_000, 100_000, 272_000, 1_000_000]`; `small_window_stage2_degrades_to_summary_only`.
- **D9** — `picks_summary_plus_slice_when_it_fits`;
  `falls_back_to_summary_only_when_the_slice_would_not_fit` (asserts **one** chat call);
  `no_savings_when_neither_candidate_shrinks`; `candidate_pick_is_tokenizer_driven_not_message_count`;
  `stage2_slice_with_a_user_interjection_row_does_not_trip_no_savings` (the `message_budget_tokens` vs
  `count_message` asymmetry from A6).
- **A9** — `declined_compaction_does_not_refire_until_the_transcript_grows` (second call gets a
  **panicking** chat callback; asserts it is never invoked); `small_window_does_not_recompact_every_turn`
  (four turns, one queued summarizer response, asserts exactly one `SUMMARIZE_INSTRUCTION` request).
  Pick a window/threshold pair where a post-compaction transcript can actually land under the gate —
  at 200/0.1 the continuation boilerplate alone is permanently over threshold.
- **D14** — a compacted message returns exactly one search hit.

**Harness knobs to add** — `with_chat_gate(Arc<Notify>)` wrapping the harness's *own* stub (today
`with_llm` replaces it outright and silently orphans `harness.stub_llm`); `compression_steps()`;
`active_transcript_texts()`.

## 7. Docs

Delete `docs/background-compression.md` and fix its 10 inbound references (`docs/modules/README.md`,
`docs/modules/context.md` ×2, `workspace/src/paths.rs`, `context/src/lib.rs`, `context/src/compressor.rs`,
`baybo/src/runtime.rs`, `store/src/session_summary.rs`, `storage/src/sqlite/mod.rs`,
`agent/src/runtime/compression.rs`).

The single home for the surviving definition is `docs/modules/context.md` §"The compaction flow"
(renamed from "The 3-stage compression flow"): stage 1 is the live summary with D9's candidate pick and
D7's relative cap, producing `[system, skill trailer, summary, verbatim recent slice]`; stage 2 is
truncate after one retry; a cancellation is a no-op. Exactly one file should carry that bracketed shape
sentence. Fix `:147` while there — it says `0.7–0.85`, the shipped default is `0.65`
(`crates/config/src/agent.rs:60`), and the same contradiction sits in `crates/context/src/lib.rs:32-34`.

One-line pointers elsewhere: `docs/modules/{trace,agent,session,storage,README}.md`, `docs/testing.md`,
`docs/turn-progress-events.md` (A4's wording, plus add iOS to the client list),
`docs/todo/trace-step-kind-audit.md`. State in `docs/modules/storage.md` that `session_summaries` stays
orphaned in old DBs with no `DROP`. **Do not touch** `docs/CONTEXT.md` (sync-protocol glossary, no
compaction vocabulary) or `docs/web-chat.md`.

Fix while here: `app/ios/CLAUDE.md:110-113` claims the iOS CI jobs are gating; both are `if: false`
(`.github/workflows/ci.yml:202,265`).

## 8. Operator runbook

Order is mandatory: **scrub → deploy**. An unscrubbed row is a hard `Step` decode failure under the new
binary (A13); a scrubbed row decodes fine under the *old* binary (`trigger: None`), so there is no
window where either side chokes. On the current live DB the scrub matches **0 rows** — `trigger` /
`applied` were only added in #222 and no running process has produced them yet — but that changes the
longer master runs before this ships.

```bash
cp ~/.baybo/state/storage.db ~/.baybo/state/storage.db.bak-rm-bgcompress
# stop baybo first: the old binary must not write new `background` rows after the scrub
sqlite3 ~/.baybo/state/storage.db <<'SQL'
UPDATE steps SET data = json_remove(data, '$.kind.trigger')
 WHERE json_extract(data, '$.kind.trigger') = 'background';
UPDATE steps SET data = json_remove(data, '$.kind.applied')
 WHERE json_extract(data, '$.kind.applied') = 'stored_summary';
SELECT count(*) FROM steps WHERE json_extract(data,'$.kind.trigger')='background';
SELECT count(*) FROM steps WHERE json_extract(data,'$.kind.applied')='stored_summary';
SQL
rm -rf ~/.baybo/state/sessions
```

`steps.job_id` is a VIRTUAL generated column over `json_extract`, so rewriting `data` recomputes it.
Repeat for the dylan container's own DB; remote-host is relay-only and has no `steps` table.

Deploy order: **install the iOS build first** (it is forward-compatible — the old server never sends a
`status` frame), then scrub, then deploy the gateway.

Local verification before opening the PR — `app/ios` has no CI coverage at all right now
(`ios-web`, `ios-core` and `ios-sim` are all `if: false`), so say so in the PR body:

```bash
cd /data/aura            && cargo clippy --all --benches --tests --examples --all-features && cargo nextest run --workspace
cd /data/aura/app/web    && pnpm type-check && pnpm test
cd /data/aura/app/ios    && cargo nextest run --workspace && cargo clippy --workspace --all-targets --all-features -- -D warnings
cd /data/aura/app/ios/web && pnpm test && pnpm build && pnpm lint
```

## 9. Risks, ranked

1. **Repeated summarizer calls.** A9's latch is the only thing between D2's larger stage-2 output and a
   full-transcript LLM call on every loop iteration. Mis-scope it (not cleared on append, or cleared by
   the trailer rebuild) and you either lose compaction entirely or reintroduce the loop. Treat the
   four-turn e2e failing as blocking.
2. **The retry doubles cost on a bad provider day.** Every transient compaction failure now costs two
   full-transcript calls, with no backoff beyond the single retry.
3. **`find_message_ordinal_by_platform_msg_id` can return the compaction-inserted copy**
   (`session.rs:1242-1248`, `ORDER BY ordinal DESC`, no filter), and that ordinal is the iOS outbox's
   rebase floor. Pre-existing; note it in the PR body, fix separately.
4. **The compaction divider's meaning loosens.** Rows just above the seam are still in context. Already
   true under stage 1 and truncate; D2 widens it. Copy fixed in both clients (§4, §5).
5. **`app/ios` has zero CI.** Every iOS claim rests on the local run.
6. **`cargo doc` is not in CI** and no crate denies broken intra-doc links, so the ~30 stale doc
   references rot silently if the §7 checklist is skipped.
