# Turn Progress Events (Live Work Display)

**Status:** ✅ Built on branch `turn-progress-events` (2026-05-26). The runtime
mechanism lives in the code (`AgentEvent` / `Frame` in `baybo-channels`, the
producers in `crates/agent/src/runtime/agent_loop.rs`) and is documented where
it renders: [`docs/modules/tui.md`](modules/tui.md) (TUI),
[`docs/modules/gateway.md`](modules/gateway.md) (wire), and
[`docs/modules/channels.md`](modules/channels.md) (`AgentOutput` shape). This
doc keeps the **design rationale** — why the forks were settled the way they
were — plus the deferred roadmap. Commit-by-commit history is in the git log.

## Problem (what this closed)

Before this feature, only **plaintext answer chunks** reached the user live —
there was no Claude/Codex-style "⏺ Read(file) ⎿ 200 lines", "Thinking…", or
running tool list. The agent had the information but it went elsewhere: the
model's **reasoning** stream was accumulated into `thinking` and never
forwarded; **tool calls** were accumulated and appended onto the *final*
message (so the TUI showed tool blocks after the answer, not as work happened);
and the **structured tool events** on `TraceEventStream` were audit-only with no
UI subscriber. `delta_tx` — the session's one ordered `AgentOutput` channel —
carried only text.

The feature turns that channel into a **curated, per-session, ordered
turn-progress event stream** so streaming channels render live work progress;
non-streaming channels drop it exactly as they drop `AnswerDelta`.

## How Claude Code / Codex do it (reference)

Both interleave, in one linear transcript: streaming **reasoning/thinking**
(dim, often collapsible), **tool invocations as they start** (`⏺ Read(path)`,
`⏺ Bash(cmd)` — the tool + a human label), a **short result line** when each
finishes (`⎿ Read 200 lines`, `⎿ exit 0`), and the final answer prose. Baybo's
twist: remote channels (Telegram/Discord/web) have no shared render surface, so
progress is **opt-in per channel** — the TUI/web render it, sidecars ignore it.

## Settled design decisions

| Decision | Resolution |
|---|---|
| **Source of truth** | Extend the existing per-session `AgentOutput` channel (`== response_tx`). `TraceEventStream` stays **audit-only**. Rationale: one ordered channel ⇒ no "progress vs final `Message`" race; progress is *curated presentation* data, distinct in shape and reveal-form from the *full-fidelity audit* spans; the sanitize/vault boundary already lives only at this channel. The trace bus is the right source for a future **operator/debug** live-trace panel — a different consumer from the conversational view. |
| **Channel shape** | `AgentOutput` is an **envelope** `{ session_id, user_id, channel, event: AgentEvent }`, so adding a variant doesn't repeat the addressing triple. The `Message(OutgoingMessage)` variant keeps the redundant inner id (it's a persisted/dispatched type with its own identity); the actor fills the envelope from it via `From<OutgoingMessage>`. |
| **`AnswerDelta` naming** | The answer-prose increment is `AnswerDelta`, not `Delta` — once `Reasoning` (also an incremental delta) sits beside it, a bare "Delta" is ambiguous. Renamed internal **and** on the wire (kind `"answer_delta"`). Layer-local names that were already distinct stay (TUI `StreamDelta`, the SDK's `onDelta` callback). |
| **Scope** | `Reasoning` (streamed thinking) + tool lifecycle (`ToolStarted` / `ToolCompleted`) + compaction status (`Status(Compacting/Compacted)`). Remaining `Status` phases (Thinking/Responding) deferred — see below. |
| **Compaction status** | `compress_if_needed` reports `Status(Compacting)` before a compaction and `Status(Compacted)` after, gated by `ContextManager::needs_compression` so the line shows **only when one actually runs**. The end edge means "the pass finished", not "the transcript changed" — it fires on a truncate fallback and a no-savings decline too, so the line never dangles. A `/stop` mid-compaction does not abort the pass, so both edges still arrive. No token-delta summary — matches the plain `/compact` confirmation. Delivered with `await` (low-frequency; the end-clear is load-bearing). |
| **Tool-lifecycle emission point** | The **agent loop** (`run_iteration`), not the executor: `ToolStarted` for every call before `join_all`, `ToolCompleted` per result after. For the common single-call iteration this is indistinguishable from per-tool timing; concurrent multi-tool batches "start together / finish together" — accepted. Per-tool real-time interleaving is a later upgrade that would move emission into `ToolExecutor::execute`. |
| **Tool label** | A dedicated `Tool::progress_label(params)` (defaults to `call_label`, exposed to the loop via `ToolRegistry::progress_label`). Kept distinct from `call_label` because that one is an *approval warning* on some tools, not a preview (Bash's `call_label` only fires on destructive commands). Tools surface their most identifying argument through the shared `baybo_tools::progress` helpers — `preview_path` (full path, left-truncated on a `/` boundary so the file name survives), `preview_arg` (whitespace-collapsed, capped at `PROGRESS_LABEL_MAX`), and `preview_search` (`<pattern> · in <path>`): Read/Write/Edit/AttachFile → the path, Bash → the command, Grep/Glob → pattern + search root, WebFetch → the URL (inherited from `call_label`), spawn_subagent → `type: summary`, CronCreate → the prompt, CronUpdate → the new title, or the id when the edit does not rename the job, Cron(Delete/Pause/Resume) → the id, Skill(Install/Uninstall) → the skill name/dir. Tools with no identifying argument (Now, CronList, dynamic MCP tools) render a bare `● tool`. |
| **Tool result summary** | Content-**light** by design (line counts, attachment/image counts, `error`/`denied`), never raw output bytes — so a leak can't ride the summary. Derived generically from `ToolOutput`; a tool-authored `Tool::result_summary` is a later refinement. |
| **Security** | `Reasoning`, `label`, and `summary` are model-/tool-derived text and pass the same sanitize + vault-reveal boundary as `AnswerDelta` (`stream_emit` / `sanitize_stream_fragment`). On a sanitize failure the summary is dropped (empty) rather than risk a leak. |
| **Ordering / backpressure** | One ordered mpsc. Answer `AnswerDelta` and tool `ToolStarted/ToolCompleted` use `await` (load-bearing / display self-consistency); `Reasoning` uses `try_send` (ephemeral, droppable) — matching how `Notice` already drops on a full channel. The final `Message` is the reconciliation point that clears any in-flight progress UI. |
| **No coalescing on the wire** | The gateway's `translator_loop` sends every `SessionEvent` 1:1 live — there is no Delta→Message coalescing (an earlier doc note claimed otherwise; it was stale). Clients without a partial surface simply ignore the streaming frames. |
| **TUI dedup** | **None needed.** `render_block` only renders the CronCreate recurring-trigger hint for `ToolUse` blocks — never general tool calls — and the wire carries no `ToolUse` to the TUI anyway, so the final `Message` is not a source of tool lines there. The scrollback `ToolStarted`/`ToolCompleted` lines are the only tool display. |
| **Approval interleave** | `ToolStarted` (loop) precedes `ApprovalRequested` (emitted inside `execute` by the gate) precedes `ToolCompleted` — naturally reads as "started → waiting for approval → done". |

## Web: reconstructed on reload

Most progress **events** are live-only and never persisted on their own — but
the web chat still shows the collapsed `Worked Xs ›` work block after a page
reload by **reconstructing** an equivalent view from the persisted *messages*.
The gateway's `api::admin::chat::reconstruct_transcript` (REST `GET
/v1/chat/sessions/:id`) folds each tool-using turn's intermediate rows
(`Thinking` → reasoning, `ToolUse` + paired `ToolResult` → tool step, mid-turn
`Text` → prose) into one `work` transcript item before the turn's final reply;
the client maps it onto the same `WorkBlock` it builds live.

The **one exception is the progress observer's narration** (`AgentEvent::Progress`,
the transient "what's happening now" line). It has no message row to be rebuilt
from, so it is persisted in its own right as a `ControlEventKind::Progress`
control event (`AgentLoop::persist_progress_narration`, anchored after the
session's newest ordinal). Unlike a notice, reconstruction does **not** give it
its own row — it folds it INTO the turn's work block as a `status` step
(`WorkStepKind::Status` / `WireWorkStepKind::Status`), the durable shadow of the
live `notice { transient: true }` frame. So a reload / reopened conversation
shows the same narration lines the live view did.

For a tab that loads **mid-turn**, reconstruction has a second source: when the
session has an active turn, `get_session` also folds the channel's live,
**not-yet-persisted** in-flight progress buffer (reasoning / answer-delta / tool
/ progress-narration steps that streamed before this tab joined) into the
trailing work block via `in_flight_work_steps()`, aligning its start with the
live `TurnState` — so the late joiner sees the steps it missed rather than an
empty in-progress block. A progress line reaches the active turn's block from
both sources at once (its persisted control event AND the live buffer), so the
fold drops the buffered `status` duplicates by text — the positioned
control-event copy wins.

Two consequences worth knowing:

- **Reconstructed tool summaries are not content-light.** Unlike the live
  `ToolCompleted.summary` (which is structural and passes
  `sanitize_stream_fragment`), the reconstructed `tool_summary` is a short
  snippet of the *raw persisted* `ToolResult`. This deliberately favors
  debugging usefulness on the bearer-gated, operator-only chat reload (never the
  live multi-channel fan-out), at the cost of possibly showing output bytes the
  live UI withheld. `tool_status` (ok/error/denied) is best-effort, keyed off the
  agent's result-formatting prefixes.
- **Mid-turn join recovery (all chat clients).** The WS never replays
  history. A (re)`Subscribe` on a live turn delivers the whole in-flight
  work block inside the `Frame::SubscribeState` bundle's `work_steps`
  half (reasoning / tool / progress-narration steps from the same
  per-session buffer), folded once by
  `channel::work_steps::in_flight_wire_steps` (the REST `ChatWorkStep`
  derives from the same `WireWorkStep`). A client that (re)subscribes
  mid-turn **replaces** its open block with it — never appends, since the
  buffer is a superset of what it saw live (progress-narration `status`
  steps included, so the REPLACE no longer drops them) — so the thinking
  it missed while backgrounded reappears without a full reload.
  A turn that **completed** while the client was away is recovered by the
  sync call (`GET /v1/chat/sessions/:id/sync`), which reconstructs closed
  work items and notices at full fidelity on every path — see
  [`docs/sync-protocol.md`](sync-protocol.md). Backward paging
  (`before_ordinal`) returns the same full-fidelity rows; a turn
  straddling a page boundary reconstructs partially until the older page
  loads (accepted partiality, same as the web reload).

## `TurnState`: the in-flight turn survives a late join

Progress events are fire-and-forget broadcasts, so a client that wasn't
connected when they fired (a second tab opened mid-turn, a reconnect) used to
have no way to tell "the agent is still working" from "the turn died without a
reply" — the web UI guessed from the transcript shape and a connect-settle
timer, mislabelling live turns as **Cancelled** and restarting the elapsed
timer at `0s`. `TurnState` replaces the guess with server truth.

The single source of truth is the **job store**: a turn is in flight exactly
when the session has a non-terminal turn job (`Job::is_turn`). `/compact` has
its own `Compact` input kind and is excluded, so it never lights up the chat as
a live reply. Background compression is not a job of its own; it records a
compression step under the triggering job. `Frame::TurnState { active,
started_at }` is that truth projected to chat clients. The actor emits
**nothing**; there is one producer of the live signal and one of the join
snapshot, both reading the same store:

- **Live edges** — the **turn-state projector** (`spawn_turn_state_projector`)
  subscribes to `JobLifecycle::subscribe_lifecycle_events`, which now carries
  both the `Pending → InProgress` **start** edge and the **terminal** edges
  (`JobPhase`). On *every* transition it recomputes
  `JobLifecycle::active_turn_started_at` for that session and broadcasts the
  current value. So both edges are derived from the same store the snapshot
  reads — they can't drift from a parallel actor emission (there is none), the
  close can't be skipped by an error or a crash, and the start carries the
  job's real `started_at`. "Recompute-is-truth" makes it robust to *which*
  job's transition fired it (the turn, a child subagent, or a `/compact` job):
  each just means "recompute this session now", and the broadcast is whatever
  is currently true.
- **Join snapshot** — the gateway sends one `SubscribeState` bundle per
  `Subscribe`, whose `turn` half reads the same `active_turn_started_at`,
  so a late joiner (new tab, reconnect) renders the in-flight turn it
  never saw start.

Because the start edge is the job's own `start()` transition (not a separate
actor emission that raced the job-row insert), a `Subscribe` can no longer land
in a window where the live signal and the snapshot disagree: from the instant
the `Pending` row exists the snapshot reads active, and the `Started` broadcast
refines `started_at` moments later.

A panicked actor is watched by `actor::runner::spawn_actor`: it delegates to
`recover_panicked_actor_session`, which closes the orphan trace rows and cancels
the orphaned turn jobs. Those terminal job events are what the projector turns
into the close edge — no special-case `TurnState` broadcast in the runner.

- **Web client**: `SessionView.turn` records the latest signal;
  `applyTurnState` reconciles the transcript tail (re-opens the
  history-reconstructed work block of an in-flight turn with the true
  `started_at`, closes the block on `active: false` even when no terminal
  `Message`/`Notice` arrives — error, cancel, blank cron reply). The
  **Cancelled** indicator renders only on a definitive `active: false`; with
  no signal yet it stays quiet.
- **iOS client**: the transcript webview folds a `status` frame into the open
  work block (`compactionStatusText`). Load-bearing beyond the label: `running`
  reads `workLive`, so an open block is what keeps the 30s `AWAITING_MAX_MS`
  backstop from flipping the composer back to send while a compaction blocks
  the turn.
- **TUI / sidecars**: drop the frame (the TUI infers turn activity locally; it
  initiated the turn).

## Out of scope / future

- **`Status` spinner — remaining phases.** `AgentEvent::Status(TurnStatus)` shipped
  for context compaction (`Compacting` / `Compacted`, see the table above). The other
  phases the enum could carry — Thinking / Responding — are still deferred on the
  **wire**. Coarse turn-level activity is no longer inferred consumer-side: that's
  `TurnState` (above). The TUI still renders a **client-side animated "working"
  indicator** (gated on a local turn-active flag — it initiated the turn) instead of
  the model's reasoning trace, and **drops `Reasoning` frames entirely** rather than
  rendering them. Other channels (e.g. the web UI) still consume `Reasoning`.
  See [`docs/modules/tui.md`](modules/tui.md#working-indicator--mid-turn-steering).
- **Relay/iOS work recovery — remaining edges** — reconnect recovery now
  rides the v2 loop on every path: in-flight work via the
  `SubscribeState` bundle, completed work + notices via the sync call,
  and backward paging at the same full fidelity. Still open: a turn
  split across a page boundary reconstructs partially until the older
  page loads (page-spanning reconstruction would need cross-page state);
  and an optional **silent-push pre-warm** to run sync before the next
  foreground.
- **Subagent → parent progress** — subagents run with `delta_tx = None`. Surfacing
  their progress to the parent ties into the planned `SubagentNotification` redesign.
- **Fine-grained in-tool events** — forward a curated subset of `ToolEventSink`
  (HTTP fetch summaries, phase timings) to the channel by switching that sink from
  buffered-drain to live-forward.
- **Operator live-trace panel** — a separate web view that subscribes to
  `TraceEventStream` directly (full audit fidelity, per-session filtering, lag
  tolerance) — the genuine home for the trace bus, distinct from this conversational
  progress view.

## Related

- `crates/channels/src/types.rs` (`AgentOutput` / `AgentEvent` / `ToolStatus`), `crates/wire/src/lib.rs` (`Frame` — its own `wire` crate, re-exported as `baybo_channels::wire`)
- `crates/agent/src/runtime/agent_loop.rs` (`chat_streaming`, `run_iteration`); `crates/agent/src/runtime/tool_executor.rs` (future per-tool emission point)
- `crates/trace/src/recorder.rs` (`TraceEventStream` — the audit bus this deliberately does *not* use)
- [`docs/mid-turn-user-interjection.md`](mid-turn-user-interjection.md) — sibling cross-cutting agent-loop feature
