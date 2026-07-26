# Background Compression — Design

A cross-cutting feature that runs the existing compression operation in a second, **background** mode in addition to the original **inline** one. The inline path (`ContextManager::maybe_compress`'s 3-stage compressor — summary.md fast-path → live LLM summary → truncate fallback) blocks the user's turn for one synchronous LLM round-trip whenever the budget threshold trips and stage 1 misses. The background path is a **detached task the session's own `AgentLoop` spawns** between LLM iterations / at end-of-job: it precomputes the summary asynchronously and persists it per-session; at compression time the inline compressor's stage-1 fast-path swaps it into the output deterministically, so the inline LLM call is skipped whenever the precomputed summary is available. Both paths share `compression::CompressionRunner` for the actual LLM dispatch; the background pass bills + traces against the session.

Affected crates: `baybo-model`, `baybo-storage`, `baybo-session`, `baybo-context`, `baybo-agent`, `baybo-workspace`.

See also: [`docs/modules/context.md`](modules/context.md), [`docs/modules/session.md`](modules/session.md), [`docs/modules/agent.md`](modules/agent.md).

## Goal

Quality-first, latency-second. Without precomputed summaries, the compressor's stage-2 LLM call fires synchronously inside the agent loop, blocking the user's turn for one LLM round-trip every time the budget threshold trips. The background path moves summary generation off the hot path so:

1. **Quality**: each summary pass refines the previous one with full transcript access; terminology stays consistent across passes; detail can grow as conversation accumulates.
2. **Latency**: at compression time, the session assembles `[system + summary + recent + skill_trailer]` from the precomputed `summary.md` — no LLM call on the hot path.

## Architecture

Two paths, decoupled, both inside the session's actor. The background pass is a detached `tokio::spawn` off the session's `AgentLoop` (mirroring `AgentLoop::spawn_session_end_write`):

```
Session (TriggerKind::User|Cron)
  AgentActor → AgentLoop
    ├─ end-of-iteration check  ──→ maybe_run_background_compression(job_done=false)
    ├─ end-of-job check        ──→ maybe_run_background_compression(job_done=true)
    │     └─ if gate passes AND no pass already in flight (in-memory JoinHandle):
    │          tokio::spawn(detached, fresh CancellationToken::new()):
    │            BackgroundCompressionRunner::run(current_job_id)
    │              → baybo_context::run_background_summary:
    │                  1. load the session's session_messages (active, ordinal ≤ up_to_ordinal)
    │                  2. read summary.md, seeding DEFAULT_NOTES_TEMPLATE if absent
    │                  3. append the session-notes prompt after the transcript
    │                  4. tool loop (≤ MAX_BACKGROUND_SUMMARY_ITERATIONS): Read/Edit
    │                     scoped to the notes path — the model rewrites summary.md
    │                     in place (same model as the session)
    │                  5. record sqlite metadata (warn-and-continue on failure)
    │          store the JoinHandle on AgentLoop.bg_compression
    └─ compress_if_needed → ContextManager::run_compression_flow
                              ├─ stage 1: load summary.md → assemble → return
                              ├─ pre-flight gate: NoOp if non_system ≤ keep_recent
                              ├─ stage 2: live LLM summary
                              └─ stage 3: truncate fallback (LLM error/empty)
```

## Data Model

### Filesystem

`crates/workspace/src/paths.rs` adds:

```
<workspace>/state/sessions/<session_id>/summary.md
```

The first-pass seed (`DEFAULT_NOTES_TEMPLATE`) is written atomically via tempfile + `rename` (mirrors `crates/workspace/src/identity.rs:36-40`); later passes rewrite the file in place through the model's `Edit` calls.

### sqlite — table `session_summaries`

```sql
CREATE TABLE session_summaries (
  session_id  TEXT    PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  cursor      INTEGER NOT NULL,           -- session_messages.ordinal at last successful pass
  pass_count  INTEGER NOT NULL DEFAULT 0,
  updated_at  INTEGER NOT NULL,           -- unix µs
  cost_micros INTEGER NOT NULL DEFAULT 0, -- per `feedback_money_no_float.md`: integer micro-USD
  model_id    TEXT    NOT NULL,
  span_id     TEXT    NOT NULL,
  error_count INTEGER NOT NULL DEFAULT 0  -- telemetry only; does NOT gate triggers
);
```

DBs created before the 2026-07 unused-column audit may additionally carry two orphan columns — `in_flight` / `in_flight_owner`, left over from the DB-flag at-most-one mechanism the in-memory `JoinHandle` replaced. Nothing reads or writes them; they stay inert there (no data migration), and fresh DBs no longer create them.

## Trigger Conditions (session side)

Two checkpoints in the session's `AgentLoop`, both gated by a 3-way conjunction:

```
fire_summary = tokens_now > 0.5 × max_tokens                   (a)
            && tokens_since_anchor > summary_diff_threshold()  (b)
            && (tool_calls_since_anchor > 3 || job_done)       (c+d)
```

Both checkpoints call `AgentLoop::maybe_run_background_compression(session, …, current_job_id, job_done)`:

| Checkpoint | Site | Disjunctive clause |
|---|---|---|
| End-of-iteration | After tool-result append, before next `compress_if_needed` (`agent_loop.rs`, `job_done = false`) | `tool_calls_since_anchor > 3` |
| End-of-job | At terminal `Final` of a `UserChat` or `Cron` turn (`agent_loop.rs`, `job_done = true`) | `job_done = true` |

The threshold evaluation itself lives in `ContextManager::maybe_request_background_summary(job_done)` — it returns `Some(BackgroundCompressionPayload { up_to_ordinal })` when the gate passes, `None` otherwise. `maybe_run_background_compression` owns the **lineage skip**, the at-most-one check, and the detached spawn.

**Subagent lineage skip.** Before either gate is measured, `maybe_run_background_compression` returns early for `is_subagent(session)` (`LineageKind::Subagent`). A subagent runs one spawned task to completion and is never resumed, so a precomputed `summary.md` — which only ever feeds a *future* inline compaction of the *same* session — can't pay off; the pass would spend an agentic Read/Edit loop (a runaway one when the model won't converge on the empty-tool-call turn) writing a summary nothing reads. Root `User`/`Cron` sessions still run it.

`up_to_ordinal` is pinned at trigger time to the **latest** `session_messages.ordinal` (`SessionManager::latest_session_ordinal`) so concurrent appends made while the pass runs don't bleed into its input window. The pass loads only the active rows at or below that ordinal.

**Anchor vs cursor.** These are deliberately different things, and conflating them is what made an unproductive pass re-fire immediately. `session_summaries.cursor` is a *coverage claim* — the newest ordinal `summary.md` actually describes — so a pass that landed no `Edit` must not advance it. `last_summary_anchor` is a *work mark* — how much new material has piled up since we last spent a pass on this session — and an unproductive pass did spend one; it just came back empty. Because the anchor was only ever synced from the cursor, such a pass advanced neither, leaving clause (b) permanently satisfied.

So an unproductive pass publishes its attempted `up_to_ordinal` to the `AgentLoop`, which `sync_anchor_to_cursor`s the anchor onto it before the next gate evaluation. The retry then has to clear the same work-proportional bar a successful pass leaves behind. There is no wall-clock component anywhere in the trigger: frequency is bounded by how much the session produced, not by how long it took to produce it, which is the only dimension that scales with what a pass costs. A transient LLM failure is deliberately *not* charged this way — it produced no work, and the documented behaviour is to retry it freely.

**Diff threshold (clause b)** is `max(SUMMARY_DIFF_TOKEN_THRESHOLD_FLOOR, SUMMARY_DIFF_CONTEXT_RATIO × max_tokens)` (`summary_diff_threshold`). A fixed absolute floor doesn't scale: the pass re-sends the whole transcript, so its cost grows with the window while the admission price stays put. On a 272K window the 5K floor let a session cross the 41K band between the background trigger and compaction in up to eight passes, each re-sending ~145K tokens to absorb ~30 new messages.

Before measuring the anchor-relative clauses (b) and (c), the gate reads `session_summaries.cursor` and calls `sync_anchor_to_cursor(meta.cursor)` to pull the in-memory anchor forward to the last successful pass. Without this, a session that crossed the 50% mark once would re-fire the background path on every later job until inline compression eventually reset the anchor. `sync_anchor_to_cursor` is monotonic (never retreats) and a no-op when the cursor isn't in the current active set.

### Anchor representation (`ContextManager`)

- **In-memory**: `last_summary_anchor: Option<usize>` — message-index in the local transcript.
- **Persistent**: `session_summaries.cursor` — `session_messages.ordinal` of the message at that anchor (for cold-start reconstruction).

### Anchor re-positioning on every compression apply

Every successful apply — all three stages — re-anchors at `messages.len()` (`ContextManager::run_compression`): the entire freshly-applied transcript, recent slice included, counts as covered, so the skill trailer / kept tail can't immediately re-trip the diff threshold.

`tool_calls_since_anchor` measures `ToolUse` blocks in `messages[anchor..]`. Fresh-installed and post-compression turns both legitimately accumulate beyond the anchor.

### Spawn serialization

At most one in-flight background pass per session, enforced **in-memory** by a `JoinHandle` field on the session's `AgentLoop` (`AgentLoop.bg_compression: Option<JoinHandle<()>>`):

```rust
// In AgentLoop::maybe_run_background_compression, before doing anything:
if let Some(handle) = self.bg_compression.as_ref()
    && !handle.is_finished()
{
    return; // a pass is already running for this session; skip the second
}
```

After a successful spawn the new `JoinHandle` is stored back on `self.bg_compression`. A finished handle (or `None`) lets the next trigger spawn afresh. The in-memory handle dies with the actor, so a process restart simply starts with `bg_compression = None`.

## The detached background pass

### Spawn (entirely inside the session's `AgentLoop`)

`maybe_run_background_compression` pre-extracts everything the `'static` task needs (the spawned future cannot borrow `&self` / `&session`), then `tokio::spawn`s the pass directly — mirroring `AgentLoop::spawn_session_end_write`:

```rust
// In AgentLoop::maybe_run_background_compression, after the gate passes
// and the at-most-one check clears:

// Fresh, never-cancelled token — NOT a child of the actor's token.
// The idle reaper cancels the actor token; deriving from it would let
// a reap mid-pass tear the summary down. Mirrors spawn_session_end_write.
let cancel_token = CancellationToken::new();

let handle = tokio::spawn(async move {
    let runner = BackgroundCompressionRunner {
        /* session_id, user_id, job_id: current_job_id, cancel_token, … */
    };
    if let Err(e) = runner.run(payload).await {
        warn!(error = %e, "background summary pass failed");
    }
});
self.bg_compression = Some(handle);
```

Key properties:

- **Attribution is the session's.** The `BackgroundCompressionRunner`'s `session_id` / `user_id` are the session's. The pass's cost row, `StepKind::Compression` step, and `LlmCall` span therefore attribute to the session/user.
- **No separate maintenance job is created.** The detached task reuses `current_job_id`, exactly like the progress observer/title generation pattern, so the background compaction appears as a `Compression` step inside the triggering job rather than as a sibling job.
- **Cancel token** is a fresh `CancellationToken::new()` that is never cancelled. It is **not** derived from the actor's token — see *Cancellation* below.

### `BackgroundCompressionRunner::run` → `baybo_context::run_background_summary`

`BackgroundCompressionRunner` (in `crates/agent/src/runtime/compression.rs`) is the agent-side adapter: it bundles the agent-layer deps (`BillableLlm`, `SpanRecorder`, `SecurityGateway`, `SessionManager`, `WorkspacePaths`, tokenizer, model info, the session identity, the minted `job_id`, and the cancel token), wraps a fresh `CompressionRunner` per LLM iteration into the context-crate callback shape, and delegates the actual flow to `baybo_context::run_background_summary`. That flow:

1. Load the session's active messages up to `up_to_ordinal` (`load_active_session_messages_up_to`).
2. Load `summary.md` from `<workspace>/state/sessions/<session_id>/summary.md`.
3. **Seed the notes file when absent** (`ensure_notes_file`): write `DEFAULT_NOTES_TEMPLATE` — the canonical section scaffold — via tempfile + rename, so the model's `Edit` calls always land against a real file.
4. Append the session-notes prompt after the transcript (`build_summary_prompt`: `PROMPT_TEMPLATE` with `{{notesPath}}` / `{{currentNotes}}` substituted, plus the size-budget appendices — see Appendix A).
5. **Run the tool loop** (at most `MAX_BACKGROUND_SUMMARY_ITERATIONS` = 10 turns): the model is offered `Read` / `Edit`, scoped by `enforce_notes_scope` to the notes path, and rewrites `summary.md` **in place** through its `Edit` calls. Tool errors come back as `ERROR:`-prefixed `tool_result` bodies so the model can retry. A **failed `Edit`** carries `FAILED_EDIT_RETRY_GUIDANCE`, which tells the model to `Read` the notes file and re-`Edit`: the prompt embeds the file once, and once an earlier `Edit` lands that snapshot is stale, so a model that dribbles edits across turns keeps missing on `old_string not found`. The prompt embeds the notes and asks for parallel `Edit`s; `Read` stays available for exactly this recovery. Each iteration calls the chat callback, which opens its own `StepKind::Compression` + `LlmCall` span via `CompressionRunner::run`. Same model as the session.

   The loop terminates on the **first** of four conditions: (a) the model responds without tool calls; (a′) **converge short-circuit** — every call in the round was an `Edit` and every one applied, which is exactly the shape the prompt asks for ("all Edit tool calls in parallel in a single message", then stop). Taking the model at its word there is what makes (a) rare: the confirmation turn it replaces costs a full transcript re-send to receive ~500 output tokens, and measured 33% of all background-summary input for 8% of the output. A round that mixed in a `Read`, or whose `Edit` errored, is still mid-recovery and gets its next turn; (b) **converge-or-stop** — `MAX_UNPRODUCTIVE_SUMMARY_ROUNDS` = 3 consecutive rounds issue tool calls but land no successful `Edit` (only `Read`s, or `Edit`s that all errored), meaning the model is thrashing and re-sending the whole transcript for nothing; (c) the hard cap `MAX_BACKGROUND_SUMMARY_ITERATIONS`, which is pathological non-termination and `record_summary_failure`s.

   Whichever way the loop exits, the pass is a **success only if at least one `Edit` landed** (`applied_any_edit`). Exits (a) and (b) with no `Edit` applied left `summary.md` byte-identical to what the pass found, so the file still covers only what the *previous* cursor covered: advancing to `up_to_ordinal` there would claim coverage the file does not have, and stage 1 would later swap that summary in and silently drop every message in the gap. Such a pass instead `record_summary_failure`s (cursor untouched) and returns `ContextError::UnproductiveSummary`, on which the agent loop advances the in-memory anchor to the attempted ordinal (see **Anchor vs cursor** above) — without that, the un-advanced cursor leaves the diff gate satisfied and the pass re-fires on the very next boundary. A "productive" round is detected by reading the `ERROR:`-prefix convention back off the `Edit` tool_result. The tolerance of 3 covers the intended recovery — a failed `Edit`, then a `Read` to refresh, then a corrected `Edit` — which spends two no-progress rounds before landing. Without (b), a model that never emits an empty-tool-call turn burns all 10 iterations — each re-sending the full (~100K+ token) transcript — before failing.
6. **sqlite metadata record** (`record_summary_success` — a single-statement upsert; no retry, a failure is logged at `warn` and the pass still succeeds, leaving an FS orphan for the startup reaper). `cursor` is `MAX()`'d rather than assigned: the pass pinned `up_to_ordinal` at trigger time but lands this row seconds to minutes later, and a compaction in that window supersedes every row it covered and `repoint_cursor`s onto the new continuation-summary row. A plain assignment drags the cursor back onto a superseded ordinal, which `lookup_anchor_index_for_cursor` can't resolve — so `tokens_since_anchor` reads the whole transcript, clause (b) is satisfied forever, and the fast path stays dead until the next pass lands. Ordinals are append-only, so the later pointer is always the live one; the condition self-heals on the next successful pass:
   ```sql
   INSERT INTO session_summaries
       (session_id, cursor, pass_count, updated_at, cost_micros, model_id, span_id, error_count)
   VALUES (?, ?, 1, ?, ?, ?, ?, 0)
   ON CONFLICT(session_id) DO UPDATE SET
       cursor = MAX(session_summaries.cursor, excluded.cursor),
       pass_count = session_summaries.pass_count + 1,
       cost_micros = session_summaries.cost_micros + excluded.cost_micros,
       error_count = 0, …;
   ```
   **Transcript elision on retry rounds.** The pinned transcript is essentially the whole cost of a pass (~145K tokens) and was re-sent verbatim on every iteration — a 2.96× amplification across the live install. Once the model has *attempted* an `Edit` (landed or not) it has already composed what it wants to write, and that text is in its own prior turn in the suffix; what it lacks is the file's current bytes, which only a fresh `Read` can supply — the embedded snapshot is exactly what went stale. So from that point the request carries `[RETRY_PROMPT_TEMPLATE, …tool-round history]` with the transcript and the original prompt dropped, and its span records `LlmCallInputs::Inline` rather than a `Persisted` reference that would no longer describe what was sent. Rounds where the model has only `Read` so far keep the full transcript: it hasn't composed anything yet, so it still needs the source material. `messages` retains the full history throughout; only the wire payload is trimmed.

7. Return a `BackgroundSummaryOutcome` (the *last* iteration's `span_id`, the summed `cost_micros`), which the spawned task logs on failure/success boundaries; the durable summary metadata remains the source of truth.

### Cancellation

The detached pass uses a **fresh `CancellationToken::new()` that is never cancelled** — deliberately decoupled from the actor's `actor_token`. The actor reaper (`AgentSupervisor::reap_idle`) cancels an idle session's `actor_token`; if the pass's token derived from it, a reap that fires mid-pass would tear down an in-flight summary and waste the LLM call. By minting a standalone token we let an in-flight pass run to completion even if the session's actor is reaped between turns. This mirrors `spawn_session_end_write`, which mints its own `JobId` and runs detached for the same reason.

The trade-off is that a process shutdown does not actively cancel an in-flight pass; the task is detached and simply abandoned when the runtime stops. That is acceptable — an abandoned pass may leave partially-applied `Edit`s in `summary.md`, but the metadata cursor never advanced, so the notes stay usable as-is and the next trigger fires fresh over the same window (the metadata write is an idempotent upsert).

### Failure handling (linear retry, no backoff)

- LLM call fails → `record_summary_failure` bumps metadata `error_count`; `summary.md` may already carry the seeded scaffold and any earlier iterations' `Edit`s; next trigger fires fresh.
- A single `Edit` / `Read` fails → **not** a pass failure: the error returns to the model as an `ERROR:`-prefixed `tool_result` for retry. A pass that applies **no** `Edit` at all *is* a failure — `record_summary_failure`, cursor untouched, `ContextError::UnproductiveSummary` (logged at `warn`), and the agent loop backs off before retrying.
- Seeding `summary.md` fails → logged at `warn` only; the default template is inlined into the prompt and the pass continues.
- Metadata update fails → file orphan; the startup FS reaper deletes orphan summary dirs whose `session_id` has no metadata row.

`error_count` is **telemetry only** — it does not gate future triggers. Acceptable cost: a persistent failure burns one LLM call per trigger event until conditions self-resolve.

## Compressor stage 1 (summary.md fast-path)

The fast-path lives as a private `try_summary_fast_path` method on `ContextManager` (see `crates/context/src/compressor.rs`). It uses the manager's existing fields — `summary_loader: FsSummaryLoader`, `sessions: Arc<SessionManager>`, `skill_registry: Arc<SkillRegistry>`, `session_id: SessionId`, `tokenizer: Arc<dyn Tokenizer>`, plus `budget.max_tokens()` for the fall-through threshold.

### Body

The fast-path **never waits** for an in-flight background pass — it reads whatever cursor + `summary.md` is currently on file and tolerates being stale-by-one. A refresh that lands just after this read simply applies on the next turn.

1. Load `session_summaries` row + `summary.md` content for `session_id`.
2. **Content check**: fall through unless `summary.md` holds something beyond the untouched `DEFAULT_NOTES_TEMPLATE` scaffold (`summary_has_content`). Independent of what the metadata row claims — step 4 is an *upper* bound, so a content-free summary is the cheapest thing that could possibly be swapped in, exactly backwards. Boilerplate is recognized by matching each line against the scaffold rather than by stripping `#` / `_…_` markup, so a model that writes into the italic descriptors instead of below them still counts as content.
3. **Cursor mapping**: resolve `metadata.cursor` to its index among the session's active rows via `SessionManager::active_index_of_ordinal` (the supersede filter is pushed into SQL, not walked client-side); that index is the cursor's position in the in-memory `messages` (full frame, including system), cross-checked against `SessionManager::count_active_messages`. Fall through if:
   - The active row count doesn't equal `messages.len()` (snapshot drift, an unpersisted system prompt, compaction in flight).
   - The cursor ordinal isn't present in the active log (compression has rewritten it).
   - The cursor maps inside the system block.
4. **Recent slice selection** (atomic-pair backward walk):
   ```
   walk backward from non_system.len() in atomic units
       (single message OR tool_use+tool_result pair):
     hard stop:    tokens + next_unit > 40K
     soft stop:    tokens ≥ 10K AND text_block_msg_count ≥ 5
   ```
   Then **clamp** the cut to be no later than the index *one past* the cursor's index in the `non_system` frame: `cut = pair_preserving_cut(non_system, min(walk_cut, cursor_idx_in_non_system + 1))`. Every message *strictly after* the cursor is unsummarized — it must remain in the recent slice — and `pair_preserving_cut` guarantees the slice never starts mid-tool-pair (so a cursor that lands on a tool_use or tool_result, e.g. an iteration-boundary trigger right after a tool exchange, doesn't produce an orphan tool_result blob). `RECENT_SLICE_MAX_TOKENS` is a *forward-extension* ceiling for the walk, not a license to drop post-cursor content.
5. **Pre-assembly threshold check** (recent slice **included**):
   - `tokens(summary) + tokens(skill_trailer) + tokens(recent_slice) > 0.6 × max_tokens` → fall through to inner. Including the recent slice catches stale-cursor scenarios where the post-cursor span alone overruns the budget.
6. **Assemble**:
   ```
   [system messages (all)]
   [user(continuation-summary message: intro + summary.md body +
         transcript pointer + footer)]
   [recent slice]
   ```
6. Return `CompressOutput::Replaced { messages }` — `ContextManager::run_compression` then inserts the skill trailer right after the system block via `insert_skill_trailer` (σ-A).

### Final transcript after fast-path apply

```
[system messages]
[user(skill reminder)]                  ← inserted by ContextManager
[user(skill detail block, optional)]
[user(continuation-summary message)]    ← intro + summary.md body +
                                          transcript pointer + footer
[recent messages, pair-preserved]
```

The continuation-summary message follows Claude Code's compaction format: a fixed `This session is being continued ...` intro, the body verbatim from `summary.md` (fast-path) or `Summary:\n<parsed body>` (stage 2), a `read the full transcript at: <path>` pointer to `<root>/logs/sessions/<session_id>.jsonl`, and a closing paragraph telling the model to resume directly without acknowledging the summary.

### Fall-through cases

Any of the following → fall through to stage 2 (LLM summary) / stage 3 (truncate fallback):
- summary.md missing (first compression on a session)
- the `active_index_of_ordinal` / `count_active_messages` lookup fails, or the active count disagrees with `messages.len()`
- the cursor ordinal isn't in the active log (compaction has rewritten it)
- the cursor maps inside the system block
- assembled `summary + recent_slice + skill_trailer` exceeds 0.6 × max_tokens
- file read / parse error

The first compression on every session pays a one-time synchronous-LLM-summary latency cost; subsequent compressions use the fast-path.

**Cursor re-point on fast-path apply.** A compaction apply supersedes every active row — including the one the cursor names — which would leave the cursor dead until the next background pass lands. For a **fast-path** apply the flow closes that window itself: `apply_session_compaction` returns the base ordinal of the new rows, and `run_compression` re-points `session_summaries.cursor` at the freshly-inserted continuation-summary row (`SessionManager::repoint_summary_cursor` — cursor + `updated_at` only, never pass_count/cost/error telemetry). This is sound because the fast-path summary row's body is `summary.md` verbatim, so the on-disk file still covers everything at or before the row; a back-to-back compaction (one giant turn leaping past the threshold, an immediate `/compact`) hits the fast path again instead of a full-transcript stage-2 call. Stage-2 / truncate applies never re-point — their output is not on disk, so an advanced cursor would claim coverage `summary.md` doesn't have, and the "cursor isn't in the active log" fall-through above remains their (recoverable) resting state.

### `force_compress` (`/compact`)

`force_compress` runs the same 3-stage flow but skips the budget threshold gate. The fast-path stage still applies when summary.md is fresh, so a user-typed `/compact` after a successful background pass reuses the cached summary instead of burning a fresh LLM call.

## Race / Concurrency Handling

| Scenario | Handling |
|---|---|
| Compression fires while refresh in-flight | Fast-path reads the last-successful summary without waiting (stale-by-one tolerated) |
| Compression fires before any summary written | Fall through to inner `Summarize` |
| Refresh writes summary.md while compression reads | The refresh's writes are the model's in-place `Edit` calls (only the first-pass template seed is tempfile+rename), so a concurrent read can see mid-pass notes — but the `session_summaries` cursor the fast-path keys on only advances via `record_summary_success` once the pass finishes |
| Two refreshes interleave on same session | In-memory `AgentLoop.bg_compression` `JoinHandle` rejects the second: a present, not-finished handle short-circuits the spawn. |
| Stale cursor (covers very old prefix) | Recent slice must cover everything after cursor; if `summary + recent + skill_trailer > 0.6 × max_tokens`, fall through |
| Process restart mid-pass | The detached task is abandoned (its `JoinHandle` lived only in the dead actor); the previous summary stays on disk and the next trigger fires fresh. A summary dir whose metadata row never committed is swept by the startup FS reaper. Any half-open `Compression` step/span under the already-completed parent job is closed by boot trace recovery's unfinished-step sweep without changing the parent job status. |

## Cold-Start Recovery (session side)

In `ContextManager::restore_from_store`:
1. Load `session_messages` (existing behaviour).
2. Load `session_summaries` row for `session_id`.
3. If row exists: resolve the index whose source `session_messages.ordinal == row.cursor`; set `last_summary_anchor = that_index + 1` — the position *after* the cursor's row, since the cursor message itself is already summarized and must not count toward `tokens_since_anchor`. The `last_synced_cursor` cache is primed at the same time.
4. If no row: `last_summary_anchor = None`.

### Orphan reaping (startup)

The background path has two startup cleanups:

- **FS orphans**: scan `<workspace>/state/sessions/*/`; for each directory name (a `session_id`), check `SELECT 1 FROM session_summaries WHERE session_id = ?`. If no row, delete the directory. This removes a summary dir left behind when a pass wrote `summary.md` (disk first) and then crashed before its metadata row committed.
- **Trace orphans**: `recover_orphaned_traces_and_jobs` asks `TraceStore::list_unfinished_steps` for half-open steps/spans. If the unfinished step belongs to a terminal parent job (the normal shape for a detached background-summary pass that outlived the turn), recovery closes the trace subtree as `SystemCrash` and leaves the completed parent job untouched.

A leftover orphan dir is otherwise harmless (the fast-path sees `summary_metadata == None` and falls through); the sweep just keeps the workspace tidy. Best-effort: errors are logged at `warn` and never block boot.

## Subagent Inheritance

- **Subagent sessions** (`LineageKind::Subagent`): **never run the background pass** — `maybe_run_background_compression` skips them via `is_subagent(session)` (see "Subagent lineage skip" under Trigger Conditions). They still compress inline if a single spawned task somehow outgrows the budget; they just never precompute a `summary.md` that would only feed a resumption they never get.

`<workspace>/state/sessions/<id>/summary.md` is strictly per-session — no symlinks, no shared paths.

## Cost Recording

Each background-summary pass:

- Wrapped in real `StepKind::Compression` + `SpanKind::LlmCall` span (same machinery as the inline `CompressionRunner`).
- The LLM call is bound to an `Attribution` whose `session_id` / `user_id` are the **session's** and whose `job_id` is the triggering job, so the cost row is charged against the **session** and joins to a `Compression` step inside that job.
- `session_summaries.cost_micros` accumulates the per-session summary-spend total (informational rollup).
- Per-pass detail is queryable via `cost_records` joined on `span_id`.

Because the spend lands directly on the session, a "summary cost for session X" report is just:

```sql
SELECT cost_micros FROM session_summaries WHERE session_id = ?;
```

(or sum the individual `cost_records` rows whose `span_id` matches the pass's `Compression` spans for that session).

## Configuration (constants for now; potential `baybo.json` knobs later)

| Constant | Value | Where |
|---|---|---|
| `SUMMARY_TRIGGER_TOKEN_THRESHOLD_RATIO` | `0.5` (× `max_tokens`) | `baybo-context` |
| `SUMMARY_DIFF_TOKEN_THRESHOLD_FLOOR` | `5_000` | `baybo-context` |
| `SUMMARY_DIFF_CONTEXT_RATIO` | `0.1` (× `max_tokens`; floor above wins on small windows) | `baybo-context` |
| `SUMMARY_TRIGGER_TOOL_CALL_THRESHOLD` | `3` | `baybo-context` |
| `RECENT_SLICE_MIN_TOKENS` | `10_000` | `baybo-context` |
| `RECENT_SLICE_MIN_TEXT_BLOCK_MSGS` | `5` | `baybo-context` |
| `RECENT_SLICE_MAX_TOKENS` | `40_000` | `baybo-context` |
| `FAST_PATH_FALLTHROUGH_THRESHOLD_RATIO` | `0.6` (× `max_tokens`) | `baybo-context` |
| `STATE_SESSIONS_SUBDIR` | `"sessions"` (under `STATE_DIR` = `"state"`) | `baybo-workspace` |
| `SUMMARY_FILE` | `"summary.md"` | `baybo-workspace` |
| `SUMMARY_FILE_TMP` | `"summary.md.tmp"` | `baybo-workspace` |

## Known Limitations

### Pattern A creep after first compression

After a fast-path or full-`Summarize` compression, the session's *active* `session_messages` no longer contains the original conversation — it contains the compressed list. Subsequent `BackgroundCompressionRunner` passes load active messages only (`superseded_by IS NULL`), so they see the embedded prior summary blob as just-another-message rather than re-deriving from original turns.

**Net effect**: Pattern B (authoritative rewrite from original transcript) is achieved on **pre-compression** passes only. Post-first-compression passes are effectively Pattern A (refine from prior summary + new turns). The prior notes still ride in the prompt's `<current_notes_content>` block and the model is told to update them "based on the user conversation above", so continuity holds — but the original pre-compression transcript is unrecoverable from the active slice.

**Why accepted**: loading all superseded rows for a long session would mean feeding 500K+ tokens to the LLM each pass — not feasible.

### Small-model caveat

Design is sized for `max_tokens ≥ ~100K`. The pre-assembly gate caps the fast-path's assembled non-system total at `0.6 × max_tokens` by construction (summary + skill trailer + recent slice), and the notes file itself is budgeted to ~12K tokens (`MAX_TOTAL_SESSION_MEMORY_TOKENS`) — so an over-budget assembly never lands. On smaller-context models (32K) that ceiling is ~19K, which the recent slice alone (up to `RECENT_SLICE_MAX_TOKENS` = 40K) can exceed: those sessions simply **fall through more often** to the inner `Summarize`, which handles them correctly. Acceptable for big-context deployments.

## Appendix A — Summary Prompt (`PROMPT_TEMPLATE`)

The background pass does **not** reuse the inline path's `SUMMARIZE_INSTRUCTION` (which lives at `crates/context/src/prompts/compression.rs` and belongs to the inline stage-2 `summarize_or_truncate` path only). It has its own prompt: `PROMPT_TEMPLATE` in `crates/context/src/background_summary.rs` — a *session-notes update* instruction, not a produce-a-summary-blob instruction.

Shape (assembled by `build_summary_prompt`, appended as the trailing user message after the transcript):

- `PROMPT_TEMPLATE` with two mustache placeholders substituted: `{{notesPath}}` (the session's `summary.md` path) and `{{currentNotes}}` (its current contents, wrapped in a `<current_notes_content>` block).
- The instruction tells the model to update the notes **based on the conversation above**, using parallel `Edit` calls against `{{notesPath}}` and nothing else, while preserving the `DEFAULT_NOTES_TEMPLATE` section scaffold verbatim (headers + italic `_descriptors_`).
- Size-budget appendices, conditionally appended: a `CRITICAL … condense` directive when the notes exceed `MAX_TOTAL_SESSION_MEMORY_TOKENS` (12K), and a per-section list when any section exceeds `MAX_SECTION_LENGTH` (2K). Both are measured with the session's own tokenizer.

The output contract is therefore **`Edit` tool calls**, not an `<analysis>` + `<summary>` text block — `parse_summary_response` plays no role in the background path.
