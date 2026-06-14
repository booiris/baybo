# Background Compression — Design

A cross-cutting feature that runs the existing compression operation in a second, **background** mode in addition to the original **inline** one. The inline path (`ContextManager::maybe_compress`'s 3-stage compressor — summary.md fast-path → live LLM summary → truncate fallback) blocks the user's turn for one synchronous LLM round-trip whenever the budget threshold trips and stage 1 misses. The background path is a **detached task the session's own `AgentLoop` spawns** between LLM iterations / at end-of-job: it precomputes the summary asynchronously and persists it per-session; at compression time the inline compressor's stage-1 fast-path swaps it into the output deterministically, so the inline LLM call is skipped whenever the precomputed summary is available. Both paths share `compression::CompressionRunner` for the actual LLM dispatch; the background pass bills + traces against the session.

Affected crates: `aura-model`, `aura-storage`, `aura-session`, `aura-context`, `aura-agent`, `aura-workspace`.

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
    │            mint JobInput::System job (attributed to PARENT session,
    │            parent_job_id = triggering turn's job)
    │              → BackgroundCompressionRunner::run → aura_context::run_background_summary:
    │                  1. load the session's session_messages (active, ordinal ≤ up_to_ordinal)
    │                  2. read summary.md (if exists)
    │                  3. one tool-free LLM call (same model as the session)
    │                  4. atomic write summary.md (tempfile + rename)
    │                  5. update libsql metadata (retry on transient failure)
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

Atomic write via tempfile + `rename` (mirrors `crates/workspace/src/identity.rs:36-40`).

### libsql — table `session_summaries`

```sql
CREATE TABLE session_summaries (
  session_id  TEXT    PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  cursor      INTEGER NOT NULL,           -- session_messages.ordinal at last successful pass
  pass_count  INTEGER NOT NULL DEFAULT 0,
  updated_at  INTEGER NOT NULL,           -- unix ms
  cost_micros INTEGER NOT NULL DEFAULT 0, -- per `feedback_money_no_float.md`: integer micro-USD
  model_id    TEXT    NOT NULL,
  span_id     TEXT    NOT NULL,
  error_count INTEGER NOT NULL DEFAULT 0  -- telemetry only; does NOT gate triggers
);
```

## Trigger Conditions (session side)

Two checkpoints in the session's `AgentLoop`, both gated by a 3-way conjunction:

```
fire_summary = tokens_now > 0.5 × max_tokens                  (a)
            && tokens_since_anchor > 5_000                     (b)
            && (tool_calls_since_anchor > 3 || job_done)       (c+d)
```

Both checkpoints call `AgentLoop::maybe_run_background_compression(session, …, current_job_id, job_done)`:

| Checkpoint | Site | Disjunctive clause |
|---|---|---|
| End-of-iteration | After tool-result append, before next `compress_if_needed` (`agent_loop.rs`, `job_done = false`) | `tool_calls_since_anchor > 3` |
| End-of-job | At terminal `Final` of a `UserChat` or `Cron` turn (`agent_loop.rs`, `job_done = true`) | `job_done = true` |

The threshold evaluation itself lives in `ContextManager::maybe_request_background_summary(job_done)` — it returns `Some(BackgroundCompressionPayload { up_to_ordinal })` when the gate passes, `None` otherwise. `maybe_run_background_compression` owns only the at-most-one check and the detached spawn.

`up_to_ordinal` is pinned at trigger time to the **latest** `session_messages.ordinal` (`SessionManager::latest_session_ordinal`) so concurrent appends made while the pass runs don't bleed into its input window. The pass loads only the active rows at or below that ordinal.

Before measuring the anchor-relative clauses (b) and (c), the gate reads `session_summaries.cursor` and calls `sync_anchor_to_cursor(meta.cursor)` to pull the in-memory anchor forward to the last successful pass. Without this, a session that crossed the 50% mark once would re-fire the background path on every later job until inline compression eventually reset the anchor. `sync_anchor_to_cursor` is monotonic (never retreats) and a no-op when the cursor isn't in the current active set.

### Anchor representation (`ContextManager`)

- **In-memory**: `last_summary_anchor: Option<usize>` — message-index in the local transcript.
- **Persistent**: `session_summaries.cursor` — `session_messages.ordinal` of the message at that anchor (for cold-start reconstruction).

### Anchor re-positioning on every compression apply

| Compression kind | New anchor |
|---|---|
| Stage 1 fast-path (summary.md hit) | `system.len() + 1 (summary blob) = recent slice start index` |
| Stage 2 LLM summary | `system.len() + 1 (summary blob)` (no recent slice) |
| Stage 3 truncate fallback | `min(prev_anchor, new_messages.len())` |

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
    let spec = JobSpec {
        session_id,                          // this session
        origin: session.trigger.kind(),      // the triggering (User / Cron) session's trigger, recorded as-is
        shape: JobShape::Maintenance,        // a compression pass, not an agent-loop turn
        input: JobInput::System { payload },
        parent_job_id: Some(current_job_id), // parent the maintenance job under the triggering turn
    };
    let result = scope::with_job(&job_lifecycle, cancel_token.clone(), spec, move |job_id| async move {
        let runner = BackgroundCompressionRunner { /* session_id, user_id, job_id, cancel_token, … */ };
        let outcome = runner.run(payload).await?;
        Ok((JobOutput::Structured { value: serde_json::to_value(&outcome)? }, outcome))
    }).await;
    if let Err(e) = result {
        warn!(error = %e, "background summary pass failed");
    }
});
self.bg_compression = Some(handle);
```

Key properties:

- **Attribution is the session's.** `JobSpec.session_id`, and the `BackgroundCompressionRunner`'s `session_id` / `user_id`, are all the session's. The pass's cost row, `StepKind::Compression` step, and `LlmCall` span therefore attribute to the session/user.
- **`origin` is the triggering session's trigger, recorded as-is** (`User` / `Cron`) — there is no payload/trigger constraint to satisfy. **`shape: JobShape::Maintenance`** is declared explicitly by this spawn path (not inferred from the `System` input) — that is what marks it as a non-turn job; the foreground `/compact` declares the same `Maintenance` shape despite a `UserChat` input. The job row's real attribution keys off `session_id` above, which is the session's.
- **`parent_job_id = Some(current_job_id)`** parents the minted maintenance job under the triggering turn's job, so the trace nests correctly.
- **Cancel token** is a fresh `CancellationToken::new()` that is never cancelled. It is **not** derived from the actor's token — see *Cancellation* below.

### `BackgroundCompressionRunner::run` → `aura_context::run_background_summary`

`BackgroundCompressionRunner` (in `crates/agent/src/runtime/compression.rs`) is the agent-side adapter: it bundles the agent-layer deps (`BillableLlm`, `SpanRecorder`, `SecurityGateway`, `SessionManager`, `WorkspacePaths`, tokenizer, model info, the session identity, the minted `job_id`, and the cancel token), wraps a fresh `CompressionRunner` per LLM iteration into the context-crate callback shape, and delegates the actual flow to `aura_context::run_background_summary`. That flow:

1. Load the session's active messages up to `up_to_ordinal` (`load_active_session_messages_up_to`).
2. Load `summary.md` from `<workspace>/state/sessions/<session_id>/summary.md` (None if absent).
3. Build `ChatRequest` (extended `SUMMARIZE_INSTRUCTION`, see Appendix A).
4. Call the chat callback — each call opens its own `StepKind::Compression` + `LlmCall` span via `CompressionRunner::run`. No tools. Same model as the session.
5. Parse response (`<analysis>` + `<summary>` block; reuse `parse_summary_response`).
6. **Atomic file write**: write to `summary.md.tmp`, fsync, rename to `summary.md`.
7. **libsql metadata update** (retry on transient failure, leave FS orphan on exhaustion):
   ```sql
   INSERT OR REPLACE INTO session_summaries
       (session_id, cursor, pass_count, updated_at, cost_micros, model_id, span_id, error_count)
   VALUES
       (?, ?, prev.pass_count + 1, ?, prev.cost_micros + ?, ?, ?, 0);
   ```
8. Return a `BackgroundSummaryOutcome`, which the spawned task serializes into `JobOutput::Structured`.

### Cancellation

The detached pass uses a **fresh `CancellationToken::new()` that is never cancelled** — deliberately decoupled from the actor's `actor_token`. The actor reaper (`AgentSupervisor::reap_idle`) cancels an idle session's `actor_token`; if the pass's token derived from it, a reap that fires mid-pass would tear down an in-flight summary and waste the LLM call. By minting a standalone token we let an in-flight pass run to completion even if the session's actor is reaped between turns. This mirrors `spawn_session_end_write`, which mints its own `JobId` and runs detached for the same reason.

The trade-off is that a process shutdown does not actively cancel an in-flight pass; the task is detached and simply abandoned when the runtime stops. That is acceptable — the pass is idempotent (atomic file write + INSERT OR REPLACE), so an abandoned pass just leaves the previous summary in place and the next trigger fires fresh.

### Failure handling (linear retry, no backoff)

- LLM call fails → metadata `error_count++`, no summary.md written, next trigger fires fresh.
- Disk write fails → metadata not updated; cost paid for nothing (logged); next trigger fires fresh.
- Metadata update fails → file orphan; the startup FS reaper deletes orphan summary dirs whose `session_id` has no metadata row.

`error_count` is **telemetry only** — it does not gate future triggers. Acceptable cost: a persistent failure burns one LLM call per trigger event until conditions self-resolve.

## Compressor stage 1 (summary.md fast-path)

The fast-path lives as a private `try_summary_fast_path` method on `ContextManager` (see `crates/context/src/compressor.rs`). It uses the manager's existing fields — `summary_loader: FsSummaryLoader`, `sessions: Arc<SessionManager>`, `skill_registry: Arc<SkillRegistry>`, `session_id: SessionId`, `tokenizer: Arc<dyn Tokenizer>`, plus `budget.max_tokens()` for the fall-through threshold.

### Body

The fast-path **never waits** for an in-flight background pass — it reads whatever cursor + `summary.md` is currently on file and tolerates being stale-by-one. A refresh that lands just after this read simply applies on the next turn.

1. Load `session_summaries` row + `summary.md` content for `session_id`.
2. **Cursor mapping**: walk `load_session_messages_with_supersede(session_id)` counting active rows (`superseded_by IS NULL`) until ordinal == `metadata.cursor`; the count is the cursor's index in the in-memory `messages` (full frame, including system). Fall through if:
   - The active row count from the supersede log doesn't equal `messages.len()` (snapshot drift, an unpersisted system prompt, compaction in flight).
   - The cursor ordinal isn't present in the active log (compression has rewritten it).
   - The cursor maps inside the system block.
3. **Recent slice selection** (atomic-pair backward walk):
   ```
   walk backward from non_system.len() in atomic units
       (single message OR tool_use+tool_result pair):
     hard stop:    tokens + next_unit > 40K
     soft stop:    tokens ≥ 10K AND text_block_msg_count ≥ 5
   ```
   Then **clamp** the cut to be no later than the index *one past* the cursor's index in the `non_system` frame: `cut = pair_preserving_cut(non_system, min(walk_cut, cursor_idx_in_non_system + 1))`. Every message *strictly after* the cursor is unsummarized — it must remain in the recent slice — and `pair_preserving_cut` guarantees the slice never starts mid-tool-pair (so a cursor that lands on a tool_use or tool_result, e.g. an iteration-boundary trigger right after a tool exchange, doesn't produce an orphan tool_result blob). `RECENT_SLICE_MAX_TOKENS` is a *forward-extension* ceiling for the walk, not a license to drop post-cursor content.
4. **Pre-assembly threshold check** (recent slice **included**):
   - `tokens(summary) + tokens(skill_trailer) + tokens(recent_slice) > 0.6 × max_tokens` → fall through to inner. Including the recent slice catches stale-cursor scenarios where the post-cursor span alone overruns the budget.
5. **Assemble**:
   ```
   [system messages (all)]
   [user(<context-summary>...summary content...</context-summary>)]
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
- supersede-log lookup fails or its active count disagrees with `messages.len()`
- the cursor ordinal isn't in the active log (compaction has rewritten it)
- the cursor maps inside the system block
- assembled `summary + recent_slice + skill_trailer` exceeds 0.6 × max_tokens
- file read / parse error

The first compression on every session pays a one-time synchronous-LLM-summary latency cost; subsequent compressions use the fast-path.

### `force_compress` (`/compact`)

`force_compress` runs the same 3-stage flow but skips the budget threshold gate. The fast-path stage still applies when summary.md is fresh, so a user-typed `/compact` after a successful background pass reuses the cached summary instead of burning a fresh LLM call.

## Race / Concurrency Handling

| Scenario | Handling |
|---|---|
| Compression fires while refresh in-flight | Fast-path reads the last-successful summary without waiting (stale-by-one tolerated) |
| Compression fires before any summary written | Fall through to inner `Summarize` |
| Refresh writes summary.md while compression reads | Atomic tempfile+rename — never partial |
| Two refreshes interleave on same session | In-memory `AgentLoop.bg_compression` `JoinHandle` rejects the second: a present, not-finished handle short-circuits the spawn. |
| Stale cursor (covers very old prefix) | Recent slice must cover everything after cursor; if `summary + recent + skill_trailer > 0.6 × max_tokens`, fall through |
| Process restart mid-pass | The detached task is abandoned (its `JoinHandle` lived only in the dead actor); the previous summary stays on disk and the next trigger fires fresh. A summary dir whose metadata row never committed is swept by the startup FS reaper. |

## Cold-Start Recovery (session side)

In `ContextManager::restore_from_store`:
1. Load `session_messages` (existing behaviour).
2. Load `session_summaries` row for `session_id`.
3. If row exists: walk loaded messages to find the index whose source `session_messages.ordinal == row.cursor`; set `last_summary_anchor = that_index`.
4. If no row: `last_summary_anchor = None`.

### Orphan reaping (startup)

The only startup cleanup the background path needs is the **FS sweep** in `reap_orphan_summaries` (`crates/agent/src/runtime/compression.rs`), run once per boot before the supervisor spawns any actors:

- **FS orphans**: scan `<workspace>/state/sessions/*/`; for each directory name (a `session_id`), check `SELECT 1 FROM session_summaries WHERE session_id = ?`. If no row, delete the directory. This removes a summary dir left behind when a pass wrote `summary.md` (disk first) and then crashed before its metadata row committed.

A leftover orphan dir is otherwise harmless (the fast-path sees `summary_metadata == None` and falls through); the sweep just keeps the workspace tidy. Best-effort: errors are logged at `warn` and never block boot.

## Subagent Inheritance

- **Subagent sessions** (`LineageKind::Subagent`): start fresh, no summary inheritance (φ-ii a). Almost never long enough to trigger; if they do, develop their own.

`<workspace>/state/sessions/<id>/summary.md` is strictly per-session — no symlinks, no shared paths.

## Cost Recording

Each background-summary pass:

- Wrapped in real `StepKind::Compression` + `SpanKind::LlmCall` span (same machinery as the inline `CompressionRunner`).
- The LLM call is bound to an `Attribution` whose `session_id` / `user_id` are the **session's** (the pass runs as a `JobInput::System` job under the session), so the cost row is charged against the **session**.
- `session_summaries.cost_micros` accumulates the per-session summary-spend total (informational rollup).
- Per-pass detail is queryable via `cost_records` joined on `span_id`.

Because the spend lands directly on the session, a "summary cost for session X" report is just:

```sql
SELECT cost_micros FROM session_summaries WHERE session_id = ?;
```

(or sum the individual `cost_records` rows whose `span_id` matches the pass's `Compression` spans for that session).

## Configuration (constants for now; potential `aura.json` knobs later)

| Constant | Value | Where |
|---|---|---|
| `SUMMARY_TRIGGER_TOKEN_THRESHOLD` | `0.5 × max_tokens` | `aura-context` |
| `SUMMARY_DIFF_TOKEN_THRESHOLD` | `5_000` | `aura-context` |
| `SUMMARY_TRIGGER_TOOL_CALL_THRESHOLD` | `3` | `aura-context` |
| `RECENT_SLICE_MIN_TOKENS` | `10_000` | `aura-context` |
| `RECENT_SLICE_MIN_TEXT_BLOCK_MSGS` | `5` | `aura-context` |
| `RECENT_SLICE_MAX_TOKENS` | `40_000` | `aura-context` |
| `FAST_PATH_FALLTHROUGH_THRESHOLD` | `0.6 × max_tokens` | `aura-context` |
| `STATE_SESSIONS_DIR` | `"state/sessions"` | `aura-workspace` |
| `SUMMARY_FILE_NAME` | `"summary.md"` | `aura-workspace` |

## Known Limitations

### Pattern A creep after first compression

After a fast-path or full-`Summarize` compression, the session's *active* `session_messages` no longer contains the original conversation — it contains the compressed list. Subsequent `BackgroundCompressionRunner` passes load active messages only (`superseded_by IS NULL`), so they see the embedded prior summary blob as just-another-message rather than re-deriving from original turns.

**Net effect**: Pattern B (authoritative rewrite from original transcript) is achieved on **pre-compression** passes only. Post-first-compression passes are effectively Pattern A (refine from prior summary + new turns). The summary prompt's "conversation is authoritative" instruction still helps because the prior summary appears verbatim in the messages and the LLM is told to use it as scaffolding — but the original transcript is unrecoverable from the active slice.

**Why accepted**: loading all superseded rows for a long session would mean feeding 500K+ tokens to the LLM each pass — not feasible.

### Small-model caveat

Design is sized for `max_tokens ≥ ~100K`. On smaller-context models (32K), the fast-path's assembled total can approach or exceed `max_tokens`, forcing immediate re-compression next turn. Acceptable for big-context deployments; smaller models fall through more often (which the existing inner `Summarize` handles correctly).

Worst-case sizing:

| Model context | Fast-path assembled total | Headroom |
|---|---|---|
| 200K | ~1K + 120K + 40K = 161K | ~20% |
| 100K | ~1K + 60K + 40K = 101K | tight |
| 32K | ~1K + 19K + 40K = 60K | overflows |

## Appendix A — Summary Prompt (extended `SUMMARIZE_INSTRUCTION`)

Prepend to the existing instruction (`crates/context/src/strategy/summarize.rs:13-103`):

```
CONTEXT: A previous summary of part of this conversation exists and is provided
below for terminology and structural consistency. The conversation transcript
above is the authoritative source — re-derive every fact from it. Only use the
prior summary as a scaffold to keep names, file paths, and concept labels stable
across passes.

PRIOR SUMMARY:
{summary.md content, or "(none — this is the first pass)"}

---

{existing SUMMARIZE_INSTRUCTION text}

SIZE TARGET: aim for ~8-12K tokens. Grow when genuinely more substance has
accumulated; do not pad.
```

Output format unchanged (`<analysis>` + `<summary>`); `parse_summary_response` keeps working.

## Appendix B — Wrapper Text for Embedded Summary

```
<context-summary>
The conversation prior to this point has been compressed for context-window
management. The summary below was produced from the full prior conversation
and represents its substantive content. Treat it as established context for
the user's current request; the recent messages that follow are the only
unsummarized exchanges.

{summary content from summary.md, verbatim}
</context-summary>
```

Role: `Role::User`.
