# Mid-Turn User Interjection (Steering)

**Status:** ✅ Implemented on branch `user_msg_insert` (2026-05-25). The
mechanism is now documented in [`docs/modules/agent.md`](../modules/agent.md)
(the scheduling section); this doc is kept for the design rationale (the settled
forks below) plus the one as-built deviation noted under "As-built notes".

## Problem

While the agent loop is running a user turn, a message the user sends mid-turn
just sits in the priority mailbox (`Trigger` priority) — the actor is blocked
`await`ing `agent_loop.run()` and isn't reading its mailbox, so the running loop
has no access to it. The message is only picked up as the **next** turn after the
current one fully completes (coalesced with any burst). Messages are never lost,
but the user cannot **steer** an in-progress turn.

We want: a message that arrives mid-turn is injected into the LLM input **after
the current tool batch finishes and before the next LLM call**, framed with a
special marker so the model knows it arrived while it was working.

### Reference: how Claude Code / Codex do it

Both do essentially this. **Codex CLI**: Enter = inject into the current turn
("submitted after the next tool call"); Tab = queue for the next turn; Esc =
interrupt now. **Claude Code**: queued messages flush at the next LLM pause
(between tool calls), with a `[steering]` tag concept. Our design matches
Codex's Enter / Claude's default. Aura's twist: remote channels (Telegram,
Discord, web, gateway-TUI) give no Enter/Tab/Esc keystroke, so we pick **one**
default policy (boundary-inject, below).

## Design Decisions (settled)

| Decision | Resolution |
|---|---|
| **Preemption** | Non-preemptive. In-flight tool/LLM call is never cancelled; `/stop` stays the only hard interrupt. Keeps the existing "a running turn is never interrupted by a higher-priority arrival" invariant. |
| **Source of truth** | The existing priority mailbox — no second queue. The loop pulls from it via a conditional pop behind a small runtime trait seam. |
| **Injection point** | Top of each iteration where `iterations > 1`, **before** `compress_if_needed`. Iteration 1 and a `Final` response never inject; anything still queued at turn-end falls to the next turn. |
| **Slash messages** | The drain stops at the first slash-leading message; it (and anything behind it) defers to the next turn, preserving the leading-`/` control contract. (`/stop` is already intercepted in the Router before the mailbox.) |
| **Marker** | Faithful persisted user-bubble row via new `MessageSource::UserInterjection`; the `<user_interjection>` envelope is applied **wire-only** in `messages_for_llm`, re-derived each call from the source flag (so it survives compaction/rebuild). |
| **Framing** | Tells the model: fold in if it refines the current task; **if unrelated, finish the current task first, then handle it** — don't restart. Acknowledge briefly. |
| **Batching** | Drain the whole leading non-slash run at the boundary; each stays its own faithful row; wrapped together under one wire envelope. |
| **Turn scope** | `UserChat` turns only — gated structurally by which handlers pass the source. A message during a `SubagentNotification` turn stays queued for the next turn (avoids that turn's in-memory rollback desyncing already-persisted rows). |
| **Provenance** | No new/mutated job; a `SpanEvent` + the persisted rows (which also appear incidentally in the next `LlmCall` span input). |
| **Rollout** | Always on, no config knob — safe fallback (unhit messages defer to next turn) + the marker handles unrelated messages. |
| **Ack** | Silent — the eventual reply acknowledges it per the framing; the user already sees their own sent message echoed in the channel. |

### Key correctness properties
- `from_user()` is the **render-as-bubble** predicate; `source() == User` exact
  is the **slash-detection** predicate. Broaden the first to include
  `UserInterjection`; leave the second exact (interjections are never slashes).
- Drained-and-persisted interjections survive turn cancel/error: once popped from
  the mailbox **and** appended to the transcript, they live in context regardless
  of turn outcome and surface next turn — never re-drained, never duplicated.
- On a user session the only `Trigger`-priority mailbox messages are `UserInput`
  (cron mints a fresh one-shot unregistered session; subagent-spawn and
  background-compression go to child sessions), so the conditional pop only ever
  faces the slash/non-slash distinction.
- Messages in the mailbox were already run through `SecurityGateway::sanitize_input`
  at Router ingress, so no re-sanitization is needed at drain time.

## Implementation Plan

### Phase 1 — `aura-model` (`crates/model/src/message.rs`)
- Add `MessageSource::UserInterjection`; extend `as_str` (`"user_interjection"`)
  and `FromStr`. New ctor `ChatMessage::user_interjection(content)` (sole producer).
- Broaden `from_user()` → `matches!(self.source, User | UserInterjection)`.

### Phase 2 — `aura-storage` (`crates/storage/src/libsql/session.rs`)
- `rehydrate_message`: add `UserInterjection => ChatMessage::user_interjection(content)`.
- No migration: existing rows never carry the new string (forward-compatible).

### Phase 3 — `aura-context`
- New `crates/context/src/prompts/interjection.rs` (mirror `cron.rs`): framing
  raw-string const + `wrap_interjections(&[String]) -> String` producing the
  `<user_interjection>…</user_interjection>` envelope.
- `crates/context/src/lib.rs`: add `frame_interjections(&[ChatMessage]) ->
  Vec<ChatMessage>` collapsing each maximal run of consecutive
  `source()==UserInterjection` rows into one enveloped `Role::User` message;
  change `messages_for_llm` to `merge_for_llm(&frame_interjections(&self.messages))`.
  Leave the two exact `MessageSource::User` filters unchanged.

### Phase 4 — mailbox (`crates/agent/src/actor/mailbox.rs`)
- Add `try_recv_if<F: FnOnce(&T) -> bool>(&mut self) -> Option<T>` — peek the top
  entry, pop iff the predicate matches.

### Phase 5 — runtime (`crates/agent/src/runtime/agent_loop.rs`)
- Define `trait InterjectionSource { fn drain_injectable(&mut self) -> Vec<Vec<ContentBlock>>; }`.
- `run`/`run_inner`: add `Option<&mut dyn InterjectionSource>`; at top of loop when
  `iterations > 1` before `compress_if_needed`, drain → `append(&ChatMessage::user_interjection(...))`.
- Emit `SpanEventKind::UserInterjection { count }` (new variant in
  `crates/trace/src/event.rs`) on the consuming iteration's LLM span + a
  structured `tracing::info!`.

### Phase 6 — actor (`crates/agent/src/actor/mod.rs`)
- `struct MailboxInterjections<'a>` impl `InterjectionSource`, draining via
  `try_recv_if(|m| matches!(m, UserInput(i) if !is_slash_command(&i.message.content)))`.
- Thread `&mut mailbox` into **`handle_user_input` + `handle_merged_user_turn` only**
  → `run_agent_loop` → `agent_loop.run`. Cron/spawned/notification pass `None`
  (this is the UserChat-only gate). Borrow-feasible: `mailbox` is a disjoint local.

### Phase 7 — render surfaces (verify, likely zero new code)
- `gateway/src/api/admin/chat.rs` (813/848), `gateway/src/channel/route.rs` (653)
  gate on `from_user()` → broadening makes interjections render as user bubbles +
  label `"user"`. API projects `source → role-string`, so no new ts-rs variant
  expected — run `scripts/check-ts-bindings.sh` to confirm.

### Phase 8 — tests, docs, gates
- Unit per phase; integration test with a fake `InterjectionSource`: inject at
  iter>1 before the LLM call; slash deferral; Final/iter-1 no-inject; cancel
  safety; batch → one envelope.
- Update `docs/modules/agent.md` scheduling-invariants section.
- Gates: `cargo fmt`; `cargo clippy --all --benches --tests --examples --all-features`
  (zero warnings); `cargo test`; `scripts/check-ts-bindings.sh`.

## Settled defaults (assumed by the plan)
`max_iterations` is **not** extended by an injection; no re-sanitization at drain;
no per-boundary cap beyond the mailbox's 4096 capacity; `reply_to` stays `None`.

## As-built notes (deviations from the plan)
- **Observability via `tracing::info!`, not a `SpanEventKind` variant.** The plan
  proposed a `SpanEventKind::UserInterjection { count }`, but `SpanEventKind` is
  ts-rs-exported (`web/src/types/trace.ts`) and rendered in the web trace UI, so a
  new variant would pull in the ts-bindings gate + a webui change — disproportionate
  for a greppability marker. As built, the drain logs a structured
  `tracing::info!(interjections = N, …)`; the durable trace record is the persisted
  `UserInterjection` rows plus their capture in the next `LlmCall` span's input.
- **Trace web UI marks interjections via the existing `source` field (no new variant).**
  The dashboard surfaces interjections without any `SpanEventKind`/ts-rs change: the
  persisted `source: "user_interjection"` (already on the `/v1/traces` wire) drives an
  "Interjected" badge on the message card (`web/src/components/trace/MessageList.tsx`,
  shown in both the session transcript and each LLM call's inputs), a per-job count chip
  in the job sidebar, and a header badge + `Interjections` Activity row in the job summary
  (`web/src/pages/TraceSessionPage.tsx`). A job is credited an interjection when a
  `user_interjection` row's `created_at` falls inside its `[started_at, ended_at)` window
  — jobs run sequentially and the row is only persisted mid-drain, so the mapping is exact.
  Vindicates the observability bullet above: the `source` flag alone was enough.
- **Only `handle_merged_user_turn` drains, not `handle_user_input`.** Every non-slash
  user message routes through `handle_merged_user_turn` (even a single message is a
  batch of one), so draining there covers the whole feature. `handle_user_input`
  (reached only for slash `/skill` turns) passes `None`, so a message sent during a
  `/skill` turn defers to the next turn — keeping the leading-`/` path simple.

## Post-review hardening (adversarial review, 2026-05-25)
- **Slash boundary could be bypassed by the in-turn drain.** The actor's coalescing
  loop used to `try_recv` the first slash command into a local `deferred` and break,
  leaving any message queued *behind* it at the mailbox head — so for a burst
  `[A, /compact, B]`, the in-turn drain pulled `B` into `A`'s turn ahead of the
  slash. Fixed by coalescing with `try_recv_if` (pop only non-slash `UserInput`s):
  the slash now stays at the mailbox **head**, serving as both the coalescing
  boundary and a barrier the drain (same predicate) cannot pop past; `deferred` is
  gone (the boundary is served by the next `recv()`). Regression:
  `slash_boundary_is_not_bypassed_by_mid_turn_drain`.
- **Wire envelope was not in the token budget.** The budget counted the raw
  interjection text, but the request carries the envelope (`messages_for_llm`), so
  the compression/budget gate under-counted by ~one envelope. Fixed: a dedicated
  `ContextManager::append_user_interjection` charges the row its framed wire size
  (non-text blocks preserved); a multi-row run over-counts slightly (safe
  direction), and `record_call_actual` resets the baseline to the provider's real
  count after the next call. Regression:
  `interjection_row_is_budgeted_at_framed_wire_size`.

## Self-review polish
- `messages_for_llm` short-circuits the framing pass (and its clone) when the
  transcript holds no interjection rows — the common case — via a cheap O(n) scan.
- Wire-size token counting is centralized in `ContextManager::message_budget_tokens`
  (see "Second review round"), so the generic `append` carries no inline special-case.
- `wrap_interjections` documents why it is **not** breakout-escaped (the content is
  the user's own message — the trusted principal of their own turn — unlike the
  untrusted `tool_output` envelope).
- Added `drained_interjection_survives_a_failed_turn` (durability across a failed turn).

## Second review round (2026-05-25)
- **TS mirror drifted.** `MessageSource` now serializes `"user_interjection"`, but the
  hand-maintained `web/src/types/trace.ts` union still listed `'user' | 'cron' | 'agent'`.
  `check-ts-bindings.sh` does NOT cover this file (it only spans the ts-rs surfaces), so the
  earlier "bindings up to date" claim missed it. The trace overview serves raw persisted rows,
  so the frontend can receive the new value. Added `'user_interjection'` to the union + doc;
  web `tsc --noEmit` clean. (`TraceSessionPage`'s `source === 'user'` genuine-prompt check is
  intentionally left exact — the TS analogue of Rust's exact `== User`.)
- **Framed counting was only on the live append path.** `restore_messages` (actor restart) and
  the compaction apply rebuilt `per_message_tokens` with the raw `count_message`, so a preserved
  `UserInterjection` row silently reverted to the un-framed count and could under-budget after a
  restart/compaction. Centralized into `ContextManager::message_budget_tokens`, now used by
  `append` **and** both rebuild paths. Regression: `restore_charges_interjection_at_framed_wire_size`.
- **Duplicated injectability predicate.** Extracted `is_coalescable_user_input`; the coalescer
  and `MailboxInterjections::drain_injectable` share it, so the slash-barrier invariant can't
  drift between the two pop sites.
- **`try_recv_if` predicate runs under the queue lock** — documented (must be a pure inspection,
  must not re-enter the mailbox).
- **Drained interjection on a *failed* turn is not re-triggered (accepted, not fixed).** Once
  drained it's persisted and surfaces on the next inbound message, but a turn that fails *after*
  the drain won't auto-run it (whereas a still-queued mailbox message would have). Kept as-is:
  the window is narrow (turn must reach a tool boundary, drain, then fail), durability holds, and
  a failed turn already returns no reply (the user re-sends, which surfaces it in-context).
  Re-enqueue-on-failure is the fix if this proves undesirable.

## Related
- `crates/agent/src/actor/mod.rs` — actor run loop, `is_slash_command`, coalescing
- `crates/agent/src/actor/mailbox.rs` — priority mailbox
- `crates/agent/src/runtime/agent_loop.rs` — `run_inner` iteration loop
- `crates/model/src/message.rs` — `MessageSource`, `from_user()`
- `crates/context/src/lib.rs` — `messages_for_llm`, `merge_for_llm`
- `crates/context/src/prompts/cron.rs` — framing-helper pattern to mirror
- `crates/storage/src/libsql/session.rs` — `rehydrate_message` source round-trip
- `docs/modules/agent.md` — non-obvious scheduling invariants
