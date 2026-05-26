# Turn Progress Events (Live Work Display)

**Status:** ✅ Built on branch `turn-progress-events` (2026-05-26). The runtime
mechanism lives in the code (`AgentEvent` / `Frame` in `aura-channels`, the
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
finishes (`⎿ Read 200 lines`, `⎿ exit 0`), and the final answer prose. Aura's
twist: remote channels (Telegram/Discord/web) have no shared render surface, so
progress is **opt-in per channel** — the TUI/web render it, sidecars ignore it.

## Settled design decisions

| Decision | Resolution |
|---|---|
| **Source of truth** | Extend the existing per-session `AgentOutput` channel (`== response_tx`). `TraceEventStream` stays **audit-only**. Rationale: one ordered channel ⇒ no "progress vs final `Message`" race; progress is *curated presentation* data, distinct in shape and reveal-form from the *full-fidelity audit* spans; the sanitize/vault boundary already lives only at this channel. The trace bus is the right source for a future **operator/debug** live-trace panel — a different consumer from the conversational view. |
| **Channel shape** | `AgentOutput` is an **envelope** `{ session_id, user_id, channel, event: AgentEvent }`, so adding a variant doesn't repeat the addressing triple. The `Message(OutgoingMessage)` variant keeps the redundant inner id (it's a persisted/dispatched type with its own identity); the actor fills the envelope from it via `From<OutgoingMessage>`. |
| **`AnswerDelta` naming** | The answer-prose increment is `AnswerDelta`, not `Delta` — once `Reasoning` (also an incremental delta) sits beside it, a bare "Delta" is ambiguous. Renamed internal **and** on the wire (kind `"answer_delta"`). Layer-local names that were already distinct stay (TUI `StreamDelta`, the SDK's `onDelta` callback). |
| **Scope (this round)** | `Reasoning` (streamed thinking) + tool lifecycle (`ToolStarted` / `ToolCompleted`). Deferred items below. |
| **Tool-lifecycle emission point** | The **agent loop** (`run_iteration`), not the executor: `ToolStarted` for every call before `join_all`, `ToolCompleted` per result after. For the common single-call iteration this is indistinguishable from per-tool timing; concurrent multi-tool batches "start together / finish together" — accepted. Per-tool real-time interleaving is a later upgrade that would move emission into `ToolExecutor::execute`. |
| **Tool label** | Reuse `Tool::call_label(params)` (the same human preview the approval prompt shows), exposed to the loop via `ToolRegistry::call_label`. Falls back to the tool name. |
| **Tool result summary** | Content-**light** by design (line counts, attachment/image counts, `error`/`denied`), never raw output bytes — so a leak can't ride the summary. Derived generically from `ToolOutput`; a tool-authored `Tool::result_summary` is a later refinement. |
| **Security** | `Reasoning`, `label`, and `summary` are model-/tool-derived text and pass the same sanitize + vault-reveal boundary as `AnswerDelta` (`stream_emit` / `sanitize_stream_fragment`). On a sanitize failure the summary is dropped (empty) rather than risk a leak. |
| **Ordering / backpressure** | One ordered mpsc. Answer `AnswerDelta` and tool `ToolStarted/ToolCompleted` use `await` (load-bearing / display self-consistency); `Reasoning` uses `try_send` (ephemeral, droppable) — matching how `Notice` already drops on a full channel. The final `Message` is the reconciliation point that clears any in-flight progress UI. |
| **No coalescing on the wire** | The gateway's `translator_loop` sends every `SessionEvent` 1:1 live — there is no Delta→Message coalescing (an earlier doc note claimed otherwise; it was stale). Clients without a partial surface simply ignore the streaming frames. |
| **TUI dedup** | **None needed.** `render_block` only renders the CronCreate recurring-trigger hint for `ToolUse` blocks — never general tool calls — so the final `Message`'s `accumulated_tool_uses` were never a visible source of tool lines in the TUI. The live `ToolStarted`/`ToolCompleted` lines are the only tool display. |
| **Approval interleave** | `ToolStarted` (loop) precedes `ApprovalRequested` (emitted inside `execute` by the gate) precedes `ToolCompleted` — naturally reads as "started → waiting for approval → done". |

## Out of scope / future

- **`Status` spinner** — `AgentEvent::Status(TurnStatus)` (Thinking / Working /
  **Compacting** / Responding); the loop would emit `Compacting` around
  `compress_if_needed`. Much of Thinking/Responding is inferable consumer-side from
  reasoning-vs-delta arrival, so this is low priority.
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

- `crates/channels/src/types.rs` (`AgentOutput` / `AgentEvent` / `ToolStatus`), `crates/channels/src/wire.rs` (`Frame`)
- `crates/agent/src/runtime/agent_loop.rs` (`chat_streaming`, `run_iteration`); `crates/agent/src/runtime/tool_executor.rs` (future per-tool emission point)
- `crates/trace/src/recorder.rs` (`TraceEventStream` — the audit bus this deliberately does *not* use)
- [`docs/mid-turn-user-interjection.md`](mid-turn-user-interjection.md) — sibling cross-cutting agent-loop feature
