# Turn Progress Events (Live Work Display)

**Status:** ✅ Built on branch `turn-progress-events` (2026-05-26), in the five
staged commits below atop this design: type reshape → wire frames → producers →
TUI render → web render. The three forks were settled with the user: (1) **extend
the `AgentOutput` channel** rather than drive UI from the trace bus; (2) scope
this round to **reasoning + tool lifecycle**; (3) emit tool lifecycle **from the
agent loop** (not the executor). The TUI rendering is also documented in
[`docs/modules/tui.md`](modules/tui.md); this doc keeps the design rationale.

## Problem

The agent has rich mid-turn information but only **plaintext answer chunks**
reach the user live — there is no Claude/Codex-style "⏺ Read(file) ⎿ 200 lines",
"Thinking…", or running tool list. Today there are three sources of
intermediate information and only one of them is user-facing:

1. **Reaches the user — text delta.** `delta_tx` is a clone of the session's one
   ordered output channel (`response_tx: mpsc::Sender<AgentOutput>`). `AgentOutput`
   (`crates/channels/src/types.rs:84`) has three variants: `Delta` (streamed
   answer text, `chat_streaming` at `agent_loop.rs:1160` forwards only
   `StreamEvent::Text` via `stream_emit`), `Notice` (out-of-band, via
   `DeltaTxNotifier` at `agent_loop.rs:126` — the `SessionNotifier` bridge, today
   only Skill risk warnings), and `Message` (final reply, sent by the **actor**).
2. **Dropped from the channel — reasoning / tool calls.** The provider stream
   (`StreamEvent`, `crates/llm/src/lib.rs:220`) yields `Text / Reasoning / ToolCall
   / ThinkingBlock / Usage`. `Reasoning` (genuinely produced, e.g. the
   openai-subscription provider) is accumulated into a `thinking` string and
   **never forwarded**; `tool_calls` are accumulated into `accumulated_tool_uses`
   (`agent_loop.rs:451`) and **appended onto the final `Message`** (`:610`) — so
   the TUI shows tool blocks *after* the answer, not as work happens.
3. **Trace-only — structured tool events.** `TraceEventStream`
   (`crates/trace/src/recorder.rs:76`, a broadcast bus) already carries
   `StepStarted/Ended`, `SpanStarted/Ended` (llm_call / tool_call with begin+result),
   `SpanEventEmitted` (`ToolEvent` Phase/HttpFetch/LlmCall), `LlmSpanEnded`
   (token counts). Its doc names "TUI / Web UI" as observers, but **no UI subscribes
   today**. Tools also emit via `ToolEventSink` (`ctx.events`), but that buffer is
   drained into the trace *after* the tool returns — not live.

Goal: turn `delta_tx` from a text pipe into a **curated, per-session, ordered
turn-progress event stream** so streaming channels render live work progress;
non-streaming channels drop it exactly as they drop `Delta` today.

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
| **Channel shape** | `AgentOutput` becomes an **envelope** `{ session_id, user_id, channel, event: AgentEvent }`, so adding a variant doesn't repeat the addressing triple. `session_id()/user_id()` read the envelope. The `Message(OutgoingMessage)` variant keeps the redundant inner id (it's a persisted/dispatched type with its own identity); the actor fills the envelope from it. |
| **Scope (this round)** | `Reasoning` (streamed thinking) + tool lifecycle (`ToolStarted` / `ToolCompleted`). Deferred: a `Status` spinner (Compacting/Iteration), subagent→parent progress, fine-grained in-tool events. |
| **Tool-lifecycle emission point** | The **agent loop** (`run_iteration`), not the executor. `ToolStarted` for every call before `join_all` (`agent_loop.rs:729`); `ToolCompleted` in the result loop after (`:733`). For the common single-call iteration this is indistinguishable from per-tool timing; concurrent multi-tool batches "start together / finish together" — accepted. Per-tool real-time interleaving is a later upgrade that would move emission into `ToolExecutor::execute`. |
| **Tool label** | Reuse `Tool::call_label(params)` (`crates/tools/src/lib.rs:59`) — the same human preview the approval prompt shows — exposed to the loop via a new `ToolRegistry::call_label(name, &params)`. Falls back to the tool name. |
| **Tool result summary** | Derived generically in the result loop from `ToolOutput`, reusing the existing match (`agent_loop.rs:735`): `Text → "N lines"`, attachments → `"+N files/images"`, `Error → status=Error`, `Err(Denied) → status=Denied`. A tool-authored `Tool::result_summary` is a later refinement. |
| **Security** | `Reasoning`, `label`, and `summary` are model-/tool-derived text and MUST pass the same sanitize + vault-reveal boundary as `Delta` (`stream_emit`). The trace keeps placeholder form; the channel gets revealed form. |
| **Ordering / backpressure** | One ordered mpsc. Answer `Delta` and tool `ToolStarted/ToolCompleted` use `await` (load-bearing / display self-consistency); `Reasoning` uses `try_send` (ephemeral, droppable) — matching how `Notice` already drops on a full channel. The final `Message` is the reconciliation point that clears any in-flight progress UI. |
| **TUI dedup** | **None needed** (confirmed during Stage 4a). `render_block` only renders the CronCreate recurring-trigger hint for `ToolUse` blocks — never general tool calls — so the final `Message`'s `accumulated_tool_uses` were never a visible source of tool lines in the TUI. The live `ToolStarted`/`ToolCompleted` lines are the only tool display; `accumulated_tool_uses` stays attached to `OutgoingMessage` untouched (other channels / the CronCreate hint still use it). |
| **Approval interleave** | `ToolStarted` (loop) precedes `ApprovalRequested` (emitted inside `execute` by the gate) precedes `ToolCompleted` — naturally reads as "started → waiting for approval → done". |
| **Wire / non-streaming channels** | New `Frame` variants mirror the events and are ts-exported (`sdks/channel-ts`). Sidecars without a partial surface ignore them, exactly as they ignore `Frame::Delta` today. |

## Event model

`crates/channels/src/types.rs` — `AgentOutput` is internal (Debug + Clone, **not**
`Serialize`); the serialized contract is `Frame`. Blast radius is therefore
`AgentOutput` + its match sites + `Frame` + the gateway adapter + TUI + web.

```rust
pub struct AgentOutput {
    pub session_id: SessionId,
    pub user_id: String,           // "" when not user-addressed (cron / system)
    pub channel: ChannelType,
    pub event: AgentEvent,
}

pub enum AgentEvent {
    Delta(String),                 // answer-prose increment (semantics unchanged)
    Reasoning(String),             // thinking increment — dim/collapsible, not answer content
    ToolStarted   { call_id: String, tool: String, label: Option<String> },
    ToolCompleted { call_id: String, status: ToolStatus, summary: String },
    Notice { level: NoticeLevel, text: String },
    Message(OutgoingMessage),      // final reply; consumers clear progress UI on this
}

pub enum ToolStatus { Ok, Error, Denied }
```

## Producers (all in `crates/agent/src/runtime/agent_loop.rs`)

- **Reasoning** — in `chat_streaming`'s match (`:1195`), the `StreamEvent::Reasoning(r)`
  arm additionally sanitizes `r` (reuse the `stream_emit` boundary) and emits
  `AgentEvent::Reasoning` via `try_send`, alongside the existing `thinking.push_str`.
- **ToolStarted** — in `run_iteration`, after `response.tool_calls` is found non-empty
  and before `join_all` (`:729`): one event per `tc` with `tool = tc.name`,
  `label = registry.call_label(&tc.name, &tc.arguments)`, `call_id = tc.id`.
- **ToolCompleted** — in the result loop (`:733`), derive `(status, summary)` from the
  `ToolOutput` match already present there.
- **Helper** — a small `emit_event(tx, session, event)` builds the envelope; tool
  lifecycle `await`s, reasoning `try_send`s.

## Transport & consumers

- **Wire** — `Frame::{Reasoning, ToolStarted, ToolCompleted}` in
  `crates/channels/src/wire.rs`; `agent_output_to_frame` (`crates/gateway/src/channel/adapter.rs:299`)
  gains the matching arms; `scripts/check-ts-bindings.sh` regenerates the SDK types.
- **TUI** — `ToolStarted` commits a cyan `⏺ tool(label)` line into native scrollback
  (`insert_before`) immediately; `ToolCompleted` commits a `⎿ summary` line coloured
  by status; `Reasoning` buffers into a dim line-buffer (`AppState.reasoning`,
  mirroring `streaming`) and commits dim `✻` lines as they form, the partial flushing
  ahead of any tool line / the answer / finalize. **No dedup needed** — `render_block`
  only renders the CronCreate hint, never general tool calls, so the final `Message`
  is not a second source of tool lines.
- **Web** — `web/src/pages/ChatPage.tsx` adds `routeInboundFrame` cases beside the
  existing `case 'delta'`: a dim reasoning row and tool chips (`⏺`/`⎿`), all
  `role: 'system'` so they never collide with the assistant-streaming reconciliation,
  and live-only (never persisted → dropped on a REST history reload).

## Staged implementation — done (branch `turn-progress-events`; each commit: `cargo clippy --all --tests` zero warnings, `cargo test` green)

1. ✅ **Type reshape** (`c26cb81`) — `AgentOutput` → envelope + `AgentEvent`; every match
   site updated. Pure mechanical, zero behaviour change.
2. ✅ **Wire + adapter + ts-export** (`22e9890`) — new `Frame` variants wired end to end;
   consumers accept-and-drop until producers land.
3. ✅ **Producers** (`39f157a8`) — reasoning forwarding + `ToolStarted/Completed` +
   `ToolRegistry::call_label` + `tool_completion_summary`; labels/summaries sanitized.
4. ✅ **Consumers** — TUI rendering (`d157faeb`) and web rendering (`94b9348`). No dedup
   was needed (see above).

## Out of scope / future

- **`Status` spinner** — `AgentEvent::Status(TurnStatus)` (Thinking / Working /
  **Compacting** / Responding); the loop would emit `Compacting` around
  `compress_if_needed`. Much of Thinking/Responding is inferable consumer-side from
  reasoning-vs-delta arrival, so this is low priority.
- **Subagent → parent progress** — subagents run with `delta_tx = None`
  (`crates/agent/src/actor/router/system_spawn/subagent.rs:29`). Surfacing their
  progress to the parent ties into the planned `SubagentNotification` redesign.
- **Fine-grained in-tool events** — forward a curated subset of `ToolEventSink`
  (HTTP fetch summaries, phase timings) to the channel by switching that sink from
  buffered-drain to live-forward.
- **Operator live-trace panel** — a separate web view that subscribes to
  `TraceEventStream` directly (full audit fidelity, per-session filtering, lag
  tolerance) — the genuine home for the trace bus, distinct from this conversational
  progress view.

## Related

- `crates/channels/src/types.rs` (`AgentOutput`), `crates/channels/src/wire.rs` (`Frame`)
- `crates/agent/src/runtime/agent_loop.rs` (`chat_streaming`, `run_iteration`, `DeltaTxNotifier`)
- `crates/agent/src/runtime/tool_executor.rs` (`execute` — future per-tool emission point)
- `crates/tools/src/lib.rs` (`Tool::call_label`, `SessionNotifier`, `ToolEventSink`)
- `crates/trace/src/recorder.rs` (`TraceEvent` / `TraceEventStream`), `crates/trace/src/event.rs` (`ToolEventPayload`)
- `crates/gateway/src/channel/adapter.rs` (`agent_output_to_frame`)
- `crates/tui/src/lib.rs` (delta / finalize), `web/src/pages/ChatPage.tsx`
- [`docs/mid-turn-user-interjection.md`](mid-turn-user-interjection.md) — sibling cross-cutting agent-loop feature
