# Mid-Turn User Interjection (Steering)

**Status:** ✅ Shipped (branch `user_msg_insert`, 2026-05-25), later extended by
the web interjection queue + atomic batching (#126). The runtime mechanism
is documented in [`docs/modules/agent.md`](modules/agent.md) (the "Long-running
model" section); this doc keeps the **design rationale** — why the forks below were
settled the way they are — plus the known limitation. Blow-by-blow
review/hardening history lives in PR #35 and the git log.

> This doc covers the **backend** boundary-inject mechanism. The operator-facing
> **web interjection queue** that sits on top of it — park / defer-to-thread /
> auto-fire on turn completion / reorder / the "send all at once" batch and the
> `/stop`-error pause banner — is documented in
> [`docs/web-chat.md`](web-chat.md) (Interjection queue).

## Problem

While the agent loop runs a user turn, a message the user sends mid-turn sits in
the priority mailbox (`Trigger`) — the actor is blocked `await`ing
`agent_loop.run()` and isn't reading its mailbox, so the running loop can't see
it. It is only picked up as the **next** turn once the current one completes.
Messages are never lost, but the user cannot **steer** an in-progress turn.

Goal: a mid-turn message is injected into the LLM input **after the current tool
batch finishes and before the next LLM call**, framed so the model knows it
arrived while it was working.

## How Claude Code / Codex do it (reference)

Both inject into the running turn at a tool boundary; **neither spawns a separate
job for an "unrelated" message** — they keep one linear thread and put the timing
choice on the human (we evaluated and rejected the new-job route; see the
`feedback`/`project` memory and the rejected-alternative note in git).

- **Codex** (verified from `openai/codex` source): Enter = inject into the
  current turn, Tab = queue for the next turn, Esc = interrupt now. A turn won't
  end while steered input is pending (`needs_follow_up || has_pending_input`); it
  has **no hard iteration cap** (relies on token-limit + auto-compaction); steering
  is bound to an explicit `active_turn`, and steering an already-ended turn bounces
  the input back to start a fresh turn.
- **Claude Code**: queued messages flush at the next LLM pause, auto-classified
  steering-vs-conversational.

Baybo's twist: remote channels (Telegram, Discord, web, gateway-TUI) have no
Enter/Tab/Esc keystroke, so we pick **one** default policy — boundary-inject,
below — and trust the model (via framing) to handle related-vs-unrelated. Unlike
Codex we keep a hard `max_iterations` cap; like Codex we are snapshot-based, so a
turn ends once the user stops sending and the model returns a no-tool `Final`.

## Settled design decisions

| Decision | Resolution |
|---|---|
| **Preemption** | Non-preemptive. The in-flight tool/LLM call is never cancelled; `/stop` stays the only hard interrupt. |
| **Source of truth** | The existing priority mailbox — no second queue. The loop pulls via a conditional pop (`try_recv_if`) behind a small runtime trait seam (`InterjectionSource`). |
| **Injection point** | Top of each iteration where `iterations > 1`, before `compress_if_needed`. Iteration 1 and a `Final` response never inject; anything still queued at turn-end falls to the next turn. |
| **Slash messages** | The drain stops at the first slash-leading message; it (and anything behind it) defers to the next turn. The slash stays at the mailbox **head** as a barrier the drain cannot pop past — it is both the coalescing boundary and the drain boundary, so a burst `[A, /compact, B]` never pulls `B` into `A`'s turn ahead of the slash. (`/stop` is intercepted in the Router before the mailbox; a `/stop`-cancelled turn additionally calls `interjections.discard_pending()` (#126) to drop the leading run of already-queued client follow-ups — including a coalesced `UserInputBatch` — so they don't auto-fire after the stop.) |
| **Marker** | Faithful persisted user-bubble row via `MessageSource::UserInterjection`; the `<user_interjection>` envelope is applied **wire-only** in `messages_for_llm`, re-derived each call from the source flag (so it survives compaction/rebuild). Not breakout-escaped — the content is the user's own message, the trusted principal of their own turn, unlike the untrusted `tool_output` envelope. |
| **Framing** | Fold in if it refines the current task; if unrelated, finish the current task first, then handle it — don't restart. Acknowledge briefly. |
| **Batching** | Drain the whole leading non-slash run; each stays its own faithful row; wrapped together under one wire envelope. |
| **Turn scope** | `UserChat` turns only, gated structurally by which handlers pass the source. Only `handle_merged_user_turn` drains — every non-slash user message routes there (a single message is a batch of one); `handle_user_input` (slash `/skill` turns) and cron/spawned/notification pass `None`. A message during a `SubagentNotification` turn stays queued (avoids that turn's in-memory rollback desyncing already-persisted rows). |
| **Budget** | `max_iterations` is **not** extended by an injection. The `UserInterjection` row is charged its **framed** wire size via `ContextManager::message_budget_tokens` — used by the live append **and** the `restore_messages` / compaction rebuild paths — so the compression gate never under-counts the envelope the request actually carries. |
| **Provenance / observability** | No new/mutated job and **no `SpanEventKind` variant** (that enum is hand-mirrored in `app/web/src/types/trace.ts` + rendered in the web trace UI — disproportionate for a greppability marker). Durable record = the persisted rows + their capture in the next `LlmCall` span input; the drain also logs `tracing::info!(interjections = N, …)`. |
| **Rollout / ack** | Always on, no config knob (safe fallback: unhit messages defer to the next turn). Silent ack — the eventual reply acknowledges it per the framing; the user already sees their sent message echoed in the channel. |

## Key correctness properties
- `from_user()` is the **render-as-bubble** predicate (broadened to include `UserInterjection`); exact `source == User` is the **slash-detection** predicate (left exact — interjections are never slashes). The web `TraceSessionPage` mirrors this: its `source === 'user'` genuine-prompt check stays exact.
- Drained-and-persisted interjections survive turn cancel/error: once popped from the mailbox **and** appended to the transcript they live in context regardless of the turn outcome and surface next turn — never re-drained, never duplicated.
- On a user session the `Trigger`-priority mailbox messages the drain faces are `UserInput`, `UserInputBatch` (#126) — the latter the web's "send all queued at once" atomic batch coalesced into one message — and `SetModel` (a mid-turn model switch; non-injectable, so it parks at the barrier like a queued slash). Cron mints a fresh one-shot session; subagent-spawn / background-compression go to child sessions. The pop predicate is shared (`is_coalescable_user_input`) between the coalescer and the drain so the barrier invariant can't drift; it accepts only single non-slash `UserInput`s — a queued `UserInputBatch` is deliberately **not** injected mid-turn (it stays queued and runs as its own next turn) and is only swept together with the leading run by `discard_pending` after a `/stop`.
- Mailbox messages were already run through `SecurityGateway::sanitize_input` at Router ingress — no re-sanitization at drain time.
- `try_recv_if`'s predicate runs under the queue lock — it must be a pure inspection and must not re-enter the mailbox.

## Trace UI
Interjections are surfaced in the web dashboard via the persisted
`source: "user_interjection"` (already on the `/v1/traces` wire — no ts-rs
change): an "Interjected" badge on the message card (`MessageList`, shown in both
the session transcript and each LLM call's inputs), a per-job count chip in the
job sidebar, and a header badge + an `Interjections` Activity row in the job
summary (`TraceSessionPage`). A job is credited an interjection when a
`user_interjection` row's `created_at` falls inside its `[started_at, ended_at)`
window — jobs run sequentially and the row is only persisted mid-drain, so the
mapping is exact.

## Known limitation
A drained interjection on a turn that **fails after** the drain is not auto-re-run:
it is persisted and surfaces on the next inbound message, but unlike a still-queued
mailbox message it won't trigger its own turn. Kept as-is — the window is narrow
(the turn must reach a tool boundary, drain, then fail), durability holds, and a
failed turn returns no reply (the user re-sends, which surfaces it in context).
Codex's "don't end a turn while input is pending" (`needs_follow_up ||
has_pending_input`) is the ready-made fix if this proves undesirable.

## Related
- `crates/agent/src/actor/mod.rs` — actor run loop, `is_slash_command`, `is_coalescable_user_input`, coalescing, `MailboxInterjections`
- `crates/agent/src/actor/mailbox.rs` — priority mailbox, `try_recv_if`
- `crates/agent/src/runtime/agent_loop.rs` — `run_inner` loop, `InterjectionSource`, `drain_user_interjections`
- `crates/model/src/message.rs` — `MessageSource`, `from_user()`, `ChatMessage::user_interjection`
- `crates/context/src/lib.rs` + `crates/context/src/prompts/interjection.rs` — `frame_interjections`, `message_budget_tokens`, `wrap_interjections`
- `crates/storage/src/sqlite/session.rs` — `rehydrate_message` source round-trip
- `app/web/src/components/trace/MessageList.tsx`, `app/web/src/pages/TraceSessionPage.tsx` — trace-UI markers
- `docs/modules/agent.md` — canonical scheduling invariants
