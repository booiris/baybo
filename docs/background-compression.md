# Background Compression — Design

A cross-cutting feature that runs the existing compression operation in a second, **background** mode in addition to the original **inline** one. The inline path (`ContextManager::maybe_compress`'s 3-stage compressor — summary.md fast-path → live LLM summary → truncate fallback) blocks the user's turn for one synchronous LLM round-trip whenever the budget threshold trips and stage 1 misses. The background path (a maintenance actor running between turns) precomputes the summary asynchronously and persists it per-session; at compression time the inline compressor's stage-1 fast-path swaps it into the output deterministically, so the inline LLM call is skipped whenever the precomputed summary is available. Both paths share `compression::CompressionRunner` for the actual LLM dispatch (cost / span attribution differs by which session / user the call is billed against).

Affected crates: `aura-model`, `aura-storage`, `aura-session`, `aura-context`, `aura-agent`, `aura-workspace`.

See also: [`docs/modules/context.md`](modules/context.md), [`docs/modules/session.md`](modules/session.md), [`docs/modules/agent.md`](modules/agent.md).

## Goal

Quality-first, latency-second. Without precomputed summaries, the compressor's stage-2 LLM call fires synchronously inside the agent loop, blocking the user's turn for one LLM round-trip every time the budget threshold trips. The new design moves summary generation to a background path so:

1. **Quality**: each summary pass refines the previous one with full transcript access; terminology stays consistent across passes; detail can grow as conversation accumulates.
2. **Latency**: at compression time, the parent assembles `[system + summary + recent + skill_trailer]` from the precomputed `summary.md` — no LLM call on the hot path.

## Architecture

Two paths, decoupled:

```
Parent session (TriggerKind::User|Cron)
  AgentActor → AgentLoop
    ├─ end-of-iteration check  ──→ maybe spawn BackgroundCompressionRunner
    ├─ end-of-job check        ──→ maybe spawn BackgroundCompressionRunner
    └─ compress_if_needed → ContextManager::run_compression_flow
                              ├─ stage 1: load summary.md → assemble → return
                              ├─ pre-flight gate: NoOp if non_system ≤ keep_recent
                              ├─ stage 2: live LLM summary
                              └─ stage 3: truncate fallback (LLM error/empty)

BackgroundCompressionRunner (TriggerKind::System, LineageKind::SystemMaintenance)
  fresh AgentActor per pass → bypasses agent_loop.run → dedicated handler:
    1. load parent's session_messages (active, ordinal ≤ N)
    2. read summary.md (if exists)
    3. one tool-free LLM call (same model as parent)
    4. atomic write summary.md (tempfile + rename)
    5. update libsql metadata (retry on transient failure)
    6. terminate
```

## Data Model

### Filesystem

`crates/workspace/src/paths.rs` adds:

```
<workspace>/state/sessions/<parent_session_id>/summary.md
```

Atomic write via tempfile + `rename` (mirrors `crates/workspace/src/identity.rs:36-40`).

### libsql — new table `session_summaries`

```sql
CREATE TABLE session_summaries (
  session_id  TEXT    PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  cursor      INTEGER NOT NULL,           -- session_messages.ordinal at last successful pass
  pass_count  INTEGER NOT NULL DEFAULT 0,
  updated_at  INTEGER NOT NULL,           -- unix ms
  cost_micros INTEGER NOT NULL DEFAULT 0, -- per `feedback_money_no_float.md`: integer micro-USD
  model_id    TEXT    NOT NULL,
  span_id     TEXT    NOT NULL,
  error_count INTEGER NOT NULL DEFAULT 0, -- telemetry only; does NOT gate triggers
  in_flight   INTEGER NOT NULL DEFAULT 0  -- 1 while a refresh pass is active for this parent
);
```

`in_flight` is the at-most-one-in-flight gate (see [Spawn serialization](#spawn-serialization)). The maintenance session row in `sessions` is kept as audit history once a pass lands and is **not** consulted by the trigger gate.

### libsql — new column on `sessions`

```sql
ALTER TABLE sessions ADD COLUMN is_normal_session BOOLEAN NOT NULL DEFAULT 1;
```

- Set to `0` at session creation when `LineageKind::SystemMaintenance`.
- Default `SessionStore` queries add `WHERE is_normal_session = 1`.
- Opt-in helpers (e.g. `list_all_sessions_including_maintenance`) include all rows.
- Does **not** propagate to spawned children (subagents from a maintenance session — currently impossible by design — would be `is_normal_session = 1`).

### `aura-model` additions

```rust
pub enum SystemReason {
    HistoryReview,
    MemoryConsolidation,
    BackgroundCompression,         // NEW
}

pub enum LineageKind {
    Subagent,
    UserFork { fork_at_job_id: JobId, prefix_state_hash: String },
    SystemMaintenance,      // NEW — no extra fields; Lineage.parent_session_id pins the for-session
}
```

`Lineage { parent_session_id, parent_job_id, kind: SystemMaintenance }` represents "this maintenance session is doing work for `parent_session_id`, anchored to `parent_job_id` (the parent's active job at trigger time)".

## Trigger Conditions (parent side)

Two checkpoints in the parent's `AgentLoop`, both gated by a 3-way conjunction:

```
fire_summary = tokens_now > 0.5 × max_tokens                  (a)
            && tokens_since_anchor > 5_000                     (b)
            && (tool_calls_since_anchor > 3 || job_done)       (c+d)
```

| Checkpoint | Site | Disjunctive clause |
|---|---|---|
| End-of-iteration | After tool-result append, before next `compress_if_needed` | `tool_calls_since_anchor > 3` |
| End-of-job | At terminal-state commit of `JobKind::UserChat` or `JobKind::Cron` (excludes `Spawned` and maintenance `System`) | `job_done = true` |

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

At most one in-flight `BackgroundCompressionRunner` per parent session. The gate consults the parent's `session_summaries.in_flight` flag:

```sql
SELECT in_flight FROM session_summaries WHERE session_id = ?
```

If `in_flight = 1`, skip the trigger; the next iteration will re-evaluate. Otherwise, the gate calls `SessionManager::mark_summary_in_flight(parent_id)` (UPSERTs a placeholder row with `in_flight = 1` if needed) before emitting the `SystemSpawnRequest`. If `try_send` fails, the gate rolls back via `clear_summary_in_flight`.

The flag is cleared by:
- `record_summary_success` / `record_summary_failure` — terminal handlers that the runner calls per pass.
- `run_background_compression`'s post-`with_job` cleanup — defensive idempotent clear that catches cancellation / panic before the runner reached `record_*`.
- Startup orphan reaper — for parents whose maintenance row was reaped (via the `bump_error_count` codepath, which also clears the flag).

Maintenance session rows in `sessions` accumulate as audit history and are no longer consulted by the gate.

## BackgroundCompressionRunner (async path)

### Spawn (parent's `AgentLoop` → `Router`)

The trigger gate does not own session creation or actor spawning. It only emits a request on a process-wide `mpsc::Sender<SystemSpawnRequest>` injected at construction time. The router's main `select!` loop owns the system-spawn arm and converts the request into a maintenance session + actor + dispatched mailbox message.

```rust
// In AgentLoop::maybe_spawn_background_compression, after the gate passes:
let request = SystemSpawnRequest::BackgroundCompression {
    parent_session_id: session.id.clone(),
    parent_job_id: current_job_id,
    parent_actor_token: self.actor_token.clone(),  // see Cancellation below
    payload: BackgroundCompressionPayload { parent_session_id, up_to_ordinal },
};
system_spawn_tx.try_send(request);  // fire-and-forget
```

```rust
// In Router::handle_system_spawn:
let maint = sessions.create_maintenance_session(&parent, parent_job_id, BackgroundCompression).await?;
let response_tx = supervisor.response_tx().clone();
let mailbox = (actor_spawner)(maint, response_tx, &parent_actor_token);
mailbox.send(AgentMessage::SystemTrigger { reason: BackgroundCompression, payload }).await;
// Sender drops; mailbox closes after the queued message is processed.
```

The one task-specific knob distinguishing this from cron is the cancel parent: the request carries the originating parent actor's `actor_token`, and the spawned maintenance child's `actor_token` derives as `parent_actor_token.child_token()`, making it a *grandchild* of the originating parent's `actor_token`. See *Cancellation* below.

`response_tx` is reused from the supervisor for constructor-shape parity. `handle_system_trigger` does not construct any `AgentOutput`, so nothing ever flows down it — the "internal-only" property is a property of the maintenance handler's body, not enforced at the type level.

### `AgentMessage::SystemTrigger`

```rust
pub enum AgentMessage {
    UserInput(...),
    CronTrigger { ... },
    SubagentSpawned { ... },
    SystemTrigger { reason: SystemReason, payload: serde_json::Value },
    Shutdown,
}
```

The actor's handler for `SystemTrigger` dispatches to `BackgroundCompressionRunner::run` directly — `agent_loop.run` is **not** invoked. The maintenance session's transcript stays empty.

### `BackgroundCompressionRunner::run`

1. Parse payload: `(parent_session_id, up_to_ordinal, previous_summary_cursor)`.
2. Load parent's messages: `sessions.load_session_messages_with_cursor(parent_id, up_to_ordinal, active_only=true)`.
3. Load `summary.md` from `<workspace>/state/sessions/<parent_id>/summary.md` (None if absent).
4. Build `ChatRequest` (extended `SUMMARIZE_INSTRUCTION`, see Appendix A).
5. Wrap in `with_job` → `StepKind::Compression` → `SpanKind::LlmCall` (mirrors `CompressionRunner` at `crates/agent/src/compression.rs`).
6. Call `llm_client.chat(request)` (no tools). Same model as parent (`π-1`).
7. Parse response (`<analysis>` + `<summary>` block; reuse `parse_summary_response` from `summarize.rs`).
8. **Atomic file write** (disk first, ρ-1 A):
   - Write to `summary.md.tmp`, fsync, rename to `summary.md`.
9. **libsql metadata update** (retry on transient failure, leave orphan on exhaustion):
   ```sql
   INSERT OR REPLACE INTO session_summaries
       (session_id, cursor, pass_count, updated_at, cost_micros, model_id, span_id, error_count)
   VALUES
       (?, ?, prev.pass_count + 1, ?, prev.cost_micros + ?, ?, ?, 0);
   ```
10. Return `JobOutput::Structured { value: { cursor, pass_count, model_id, span_id } }`.

### Cancellation (C2)

Parent's `actor_token` cancellation propagates to its maintenance children automatically via the `tokio_util` `CancellationToken` tree.

The trigger gate carries a **clone of the parent's `actor_token`** in every `SystemSpawnRequest`. The router passes that token to the actor spawner factory as the new maintenance actor's `parent_token`. `AgentActor::new` then derives the maintenance actor's own `actor_token` as a `child_token()` of that. Because cancellation in `tokio_util` cascades through the tree, calling `parent.actor_token.cancel()` (which the parent's `Shutdown` handler does) automatically trips the maintenance child's `actor_token`, which in turn trips every nested job/tool token derived from it.

No explicit "Shutdown the maintenance children" step is required — and no per-parent mailbox map needs to be maintained on the spawner side. The parent simply cancels its own token; the cascade reaches every descendant.

### Failure handling (linear retry, no backoff)

- LLM call fails → metadata `error_count++`, no summary.md written, next trigger fires fresh.
- Disk write fails → metadata not updated; cost paid for nothing (logged); next trigger fires fresh.
- Metadata update fails → file orphan; cold-start orphan reaper deletes orphan files whose `session_id` has no metadata row.

`error_count` is **telemetry only** — it does not gate future triggers. Acceptable cost: a persistent failure burns one LLM call per trigger event until conditions self-resolve.

## Compressor stage 1 (summary.md fast-path)

The fast-path lives as a private `try_summary_fast_path` method on `ContextManager` (see `crates/context/src/compressor.rs`). It uses the manager's existing fields — `summary_loader: FsSummaryLoader`, `sessions: Arc<SessionManager>`, `skill_registry: Arc<SkillRegistry>`, `session_id: SessionId`, `tokenizer: Arc<dyn Tokenizer>`, plus `budget.max_tokens()` for the fall-through threshold.

### Body

0. **Wait for in-flight settle** (mirrors Claude Code's `waitForSessionMemoryExtraction`): poll `session_summaries.in_flight` for up to `SUMMARY_REFRESH_WAIT_TIMEOUT` (15s, see [Configuration](#configuration)) at `SUMMARY_REFRESH_WAIT_POLL_INTERVAL` (250ms). If a background pass lands during the wait, the next step picks up the fresher cursor; on timeout the wrapper proceeds with whatever metadata is on file (stale-by-one tolerated). Bounded so a stuck refresh can't block a user turn indefinitely.
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
[user(<context-summary>summary.md content</context-summary>)]
[recent messages, pair-preserved]
```

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
| Compression fires while refresh in-flight | Use last-successful summary (stale-by-one tolerated) |
| Compression fires before any summary written | Fall through to inner `Summarize` |
| Refresh writes summary.md while compression reads | Atomic tempfile+rename — never partial |
| Two refreshes interleave on same parent | `session_summaries.in_flight` flag rejects the second; gate cleared by `record_summary_*` or runner cleanup |
| Stale cursor (covers very old prefix) | Recent slice must cover everything after cursor; if `summary + recent + skill_trailer > 0.6 × max_tokens`, fall through |
| Cold-start orphans (process crash mid-pass) | Startup reaper deletes leftover maintenance rows, calls `record_summary_failure` (which clears `in_flight`) on each parent |

## Cold-Start Recovery (parent side)

In `ContextManager::restore_from_store`:
1. Load `session_messages` (existing behaviour).
2. Load `session_summaries` row for `session_id`.
3. If row exists: walk loaded messages to find the index whose source `session_messages.ordinal == row.cursor`; set `last_summary_anchor = that_index`.
4. If no row: `last_summary_anchor = None`.

### Orphan reaping (startup)

- **DB orphans**: `UPDATE sessions SET state = 'Failed' WHERE state = 'Active' AND is_normal_session = 0`.
- **FS orphans**: scan `<workspace>/state/sessions/*/summary.md`; for each parent_id, check `SELECT 1 FROM session_summaries WHERE session_id = ?`. If no row, delete the file (tempfile cleanup if `.tmp` present).

### Crash → parent error_count

When a maintenance session is marked `Failed` on startup, increment `session_summaries.error_count` for its `parent_session_id`. Conservative: prevents a crash loop on a doomed conversation from quietly burning summary calls every restart.

## Fork / Subagent Inheritance

- **Forked sessions** (`LineageKind::UserFork`): start fresh, no summary inheritance (φ-i b). Develop their own summary lifecycle if they grow long enough to trigger.
- **Subagent sessions** (`LineageKind::Subagent`): start fresh, no summary inheritance (φ-ii a). Almost never long enough to trigger; if they do, develop their own.

`<workspace>/state/sessions/<id>/summary.md` is strictly per-session — no symlinks, no shared paths.

## Cost Recording

Each `BackgroundCompressionRunner` pass:

- Wrapped in real `StepKind::Compression` + `SpanKind::LlmCall` span (same machinery as existing `CompressionRunner`).
- `CostManager::record_call(span_id, ...)` charged against the **System session** (not the parent).
- `session_summaries.cost_micros` accumulates per-parent total (informational rollup).
- Per-pass detail queryable via `cost_records` joined on `span_id`.

A future "summary cost for session X" report joins:

```sql
SELECT SUM(cost_records.cost_micros)
FROM sessions
JOIN cost_records ON sessions.id = cost_records.session_id
WHERE sessions.lineage_for_session_id = ?
  AND sessions.lineage_kind = 'system_maintenance';
```

(or simpler: read `session_summaries.cost_micros` directly).

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
| `SUMMARY_REFRESH_WAIT_TIMEOUT` | `15s` | `aura-context` |
| `SUMMARY_REFRESH_WAIT_POLL_INTERVAL` | `250ms` | `aura-context` |
| `STATE_SESSIONS_DIR` | `"state/sessions"` | `aura-workspace` |
| `SUMMARY_FILE_NAME` | `"summary.md"` | `aura-workspace` |

## Known Limitations

### Pattern A creep after first compression

After a fast-path or full-`Summarize` compression, the parent's *active* `session_messages` no longer contains the original conversation — it contains the compressed list. Subsequent `BackgroundCompressionRunner` passes load active messages only (`superseded_by IS NULL`), so they see the embedded prior summary blob as just-another-message rather than re-deriving from original turns.

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
