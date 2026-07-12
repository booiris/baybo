# Chat Sync Protocol v2 — One Cursor, One Sync

**Status:** ✅ Built (2026-07-06; designed on branch
`docs/chat-sync-protocol-v2`, implemented in `20d50454` on `feat/ios-swiftui`,
merged to master) — reviewed (codex + code-grounded verification) with all
launch open questions resolved (see the Decision log), then implemented as one
atomic cut-over series: the `sync` endpoint + `platform_msg_id` point lookup,
`Frame::SubscribeState` / `Frame::Gap`, the Subscribe scoping fix and
channel-scoped patch broadcasts, and deletion of the WS replay, REST catch-up,
`Frame::Reset` / `WorkSnapshot` / `PendingApprovalsSnapshot`, and the client
sentinel/hydration machinery. This doc distills a full-codebase audit of the
v1 web/iOS message-sync paths plus a survey of how production IM systems
(WeChat, Telegram, Matrix, DingTalk, RongCloud) solve the same problem.
**Current-state file references below describe the retired v1 code** as of
branch `feat/ios-swiftui` @ `811a9847` — read them as the audit record that
motivated the design, not as live pointers.

Related docs: [`docs/web-chat.md`](web-chat.md) (current web data flow),
[`docs/turn-progress-events.md`](turn-progress-events.md) (streaming frames +
work-block reconstruction), [`docs/modules/gateway.md`](modules/gateway.md)
(wire + REST surface), [`docs/modules/channels.md`](modules/channels.md)
(channel kinds, dispatch), `app/ios/CLAUDE.md` (the v2 iOS transcript-sync
loop; the seven-cell hydration matrix this doc proposed deleting is gone).

## Problem

A transcript client can fall behind in exactly one way — it is missing
persisted rows after some point — yet baybo currently has **five mechanisms**
that each recover a different slice of that one gap, and the two clients
combine them differently:

| Mechanism | Fidelity | Who uses it |
|---|---|---|
| WS `Subscribe { since_ordinal }` replay (`route.rs:393-395`, cap 200 → `Reset`) | messages only | web, iOS (the TUI sends the field but always `None` — it never replays, `tui/src/client/ws.rs:169-178`) |
| REST `GET …/catch-up` (`chat.rs:854-924`) | messages + work, **no notices**; `truncated` returns nothing | iOS only |
| REST `GET /v1/chat/sessions/{id}` (`chat.rs:728-852`) | full fidelity (messages + work + notices) | web hydrate/paging; iOS paging **after an FFI filter strips work/notices** (`gateway_api.rs:197-206`) |
| `Frame::Reset` | "give up, refetch" | web wipes **every** session view (`ChatPage.tsx:848-875`); iOS rebuilds one session at message-only fidelity |
| iOS mirror + `listed` flag + `ensureBackfilled` + hydration matrix (cells A–G, `app/ios/CLAUDE.md`) | whatever the mirror had | iOS only |

The split is not hypothetical cost. The 2026-07-06 audit verified these
user-visible defects, all downstream of the mechanism split:

1. **Web flap hole** — after an ordinary reconnect (gap ≤ 200, no Reset),
   turns completed during the gap render as bare bubbles: WS replay is
   message-only and web never calls catch-up (types generated in
   `schema.d.ts`, zero call sites). Work cards and notices are gone until a
   full reload.
2. **iOS rebuild hole** — every reset-REPLACE path (mirror-less backfill,
   `Frame::Reset`, `truncated` escalation) funnels through the FFI filter that
   drops `kind != "message"`, then immediately overwrites the mirror, so
   historical work blocks and notices are erased *durably*. `811a9847` made
   this the first-paint path for every mirror-less open.
3. **Cursor sentinel drift** — three "no cursor" encodings coexist: `None`
   (replay nothing), `-1` (web: replay everything), `0` (iOS: skips ordinal 0
   — an off-by-one with two verified first-message-loss cells, since replay is
   strictly `ordinal > n` and the first row gets ordinal 0).
4. **Reset over-reach** — `Frame::Reset` carries no `session_id`, so web can
   only respond by wiping all sessions and refetching.

Each of these was individually patchable, but the patches (the hydration
matrix is seven cells deep) keep accumulating because the underlying shape is
wrong: recovery fidelity depends on *which* mechanism happened to fire.

## Prior art — what production IM systems converge on

Four independent designs (WeChat SyncKey/seqsvr, Telegram `pts` +
`getDifference`, Matrix `/sync`, and the DingTalk/RongCloud/Tencent-IM
pattern literature) converge on the same skeleton:

1. **Server-authoritative store, one monotonic sequence per box.** Monotonic,
   *not necessarily contiguous* (WeChat's seqsvr explicitly allows jumps).
   Holding cursor S means "I have everything ≤ S in this box".
2. **Push is a signal; pull is the truth.** Pushes (long-connection notify,
   APNs/FCM) exist for latency only. Consistency is always restored by the
   client pulling `sync(my cursor)`. RongCloud clients even re-pull every 3–5
   minutes as a lost-push safety net.
3. **One cursor, one sync loop, every scenario.** Cold start, reconnect,
   push-triggered wake, offline catch-up, multi-device: all the *same* call.
   WeChat's own account: offline delivery and online delivery are one state
   sync operation — there is no separate "offline message" subsystem. The
   cursor advances only when the client presents it on the next request, so
   the loop is at-least-once with idempotent apply and needs **no downstream
   ack protocol**.
4. **Two-tier gap handling.** Small gap: pull the difference (buffering live
   pushes while the pull is in flight — Telegram spells this out). Large gap:
   **rebase** — `updates.differenceTooLong` / DingTalk's Rebase event / Matrix
   `limited: true` + `prev_batch`: abandon incremental replay, adopt the
   newest page as the new baseline, backfill older history lazily on scroll.

Send-side standard kit: a client-generated idempotency key that the server
dedups on (Telegram `random_id` never expires; Matrix puts `txnId` in the URL
path and replays the original response), a persisted outbox with a
sending → sent → failed state machine and automatic retry, and
**pull-as-reconciliation** — Tencent IM documents that pulling roaming history
overwrites locally-marked-failed messages, resolving the "ack lost but send
succeeded" ambiguity without user action.

References: [Telegram updates](https://core.telegram.org/api/updates) ·
[seqsvr paper](https://cloud.tencent.com/developer/article/1004444) ·
[WeChat backend evolution (张文瑞, InfoQ)](https://www.infoq.cn/article/the-road-of-the-growth-weixin-background) ·
[Matrix /sync](https://spec.matrix.org/latest/client-server-api/) ·
[MSC4186 sliding sync](https://github.com/matrix-org/matrix-spec-proposals/blob/main/proposals/4186-simplified-sliding-sync.md) ·
[DTIM deep-dive](https://www.cnblogs.com/imteck4713/p/16587859.html) ·
[Tablestore IM architecture](https://developer.aliyun.com/article/253242) ·
[RongCloud delivery](https://cloud.tencent.com/developer/article/1852949) ·
[Tencent IM message FAQ](https://cloud.tencent.com/document/product/269/32486)

baybo already has the hard part: `session_messages.ordinal` **is** a correct
per-box sequence (dense, monotonic, actor-serialized, assigned at persist —
`crates/storage/src/libsql/session.rs:515-519`). Per-session ordinals are
exactly Telegram's per-channel `pts` model. What's missing is the discipline
around it.

## Design principles

1. **One cursor.** `session_messages.ordinal`, unchanged. A client persists at
   most one cursor per session. "No cursor" is not a special mode — it is
   `since = null`, meaning "baseline me on the newest page".
2. **One sync call.** All forward recovery — cold start, reconnect, buffer
   overflow, push tap, gap nudge — is the same request. Fidelity is a property
   of the *data*, never of the *path* that fetched it.
3. **Push is a signal, pull is the truth.** Live frames and APNs accelerate;
   sync guarantees. Anything ephemeral that is dropped is recoverable from
   sync or is explicitly fire-and-forget (below).
4. **Three planes, stated explicitly:**
   - **Durable plane** — persisted transcript rows (message / work / notice),
     addressed by ordinal, delivered by sync and by live `Message` frames
     (the final assistant reply ordinal-stamped; the user echo ordinal-less
     by decision — see the echo section). Never lossy.
   - **State plane** — idempotent REPLACE snapshots of *current* session
     state: turn activity, in-flight work steps, pending approvals, task
     list. Delivered as one bundle on subscribe; updated by live frames.
     Losing one is repaired by the next snapshot, not by history.
   - **Ephemeral plane** — `AnswerDelta` / `Reasoning` / tool lifecycle /
     transient notices / `SessionActivity`. Droppable by design; a client
     that missed them recovers the *outcome* via the other two planes.

   This is Matrix's timeline / state / ephemeral split, adapted to the one
   thing baybo has that classic IM doesn't: an LLM turn is a long-running
   transaction whose intermediate output matters live but is only *summarized*
   durably (the reconstructed work block).
5. **The client is dumb.** All diff/reconstruction logic lives server-side
   (the WeChat SyncKey philosophy — this is what keeps N clients behaviorally
   identical). Web and iOS run the *same* algorithm; the iOS mirror is a pure
   cache of it.

## The protocol

### Durable plane: the `sync` call

```
GET /v1/chat/sessions/{id}/sync?since_ordinal=<i64|absent>&limit=<n≤200>
→ {
    rows: [TranscriptRow],        // kind: message | work | notice — the
                                  // full-fidelity DTO get_session already
                                  // returns (chat.rs:2047+), verbatim
    next_cursor: i64 | null,      // COVERAGE watermark: highest persisted
                                  // ordinal the scan covered (visible or
                                  // not); null iff the session is empty
    rebased: bool,                // true ⇒ rows are the NEWEST page, not the
                                  // requested difference (gap > limit)
    oldest_ordinal: i64 | null,   // page floor, for backfill
    has_more_older: bool          // prev_batch analogue
  }
```

Semantics:

- `since_ordinal` absent → newest page (`history_tail`), `rebased: false`.
  This *is* the cold-start / mirror-less / fresh-install path. No `listed`
  flag, no backfill decision tree — opening any session always issues sync.
- `since_ordinal = n` → rows strictly `> n` (`history_since`), ascending.
- `next_cursor` is the coverage watermark (newest persisted ordinal
  *scanned*, visible or not) — otherwise every sync re-scans the invisible
  tool/system tail. The **rebase test counts emitted transcript rows**
  against `limit`, not scanned rows: an agentic turn persists hundreds of
  invisible tool rows per handful of visible ones, and counting scanned rows
  would force a mid-stream REPLACE under a watching user. The server keeps
  scanning past invisible rows up to an implementation scan bound (suggested
  10×limit; hitting the bound also rebases). Clients advance
  `cursor = max(cursor, next_cursor)`; live final-reply ordinals feed the
  same max — with one exception: **a rebased page dirties the cursor** (next
  bullet) — and replay is idempotent.
- **Rebase-dirty cursor + REPLACE overlay** (closes a permanent-hole window
  found in verification): after applying a rebased page, only a sync
  `next_cursor` may advance the cursor until one non-rebased sync completes
  — live final-Message ordinals render but do not advance it. Otherwise a
  row persisted *after* the page was built but *before* the turn's final
  reply (the mid-turn interjection window: its echo already fired, so
  nothing ever re-delivers it live) would be leapfrogged forever by the
  strictly-`> cursor` select — and on iOS the mirror would persist the hole
  durably. The follow-up sync fires on turn end and on the safety tick. The
  REPLACE itself must also **re-overlay locally-rendered rows whose
  `platform_msg_id` is still in the outbox** and absent from the page —
  their content lives in the retained outbox entry (outbox rule 2); an
  echoed-but-unpersisted interjection would otherwise vanish from screen
  with nothing left to restore it.
- Control events (notices, slash echoes) are NOT ordinal-addressed — they
  live in `session_control_events(seq, after_ordinal)`, and a notice can be
  written later with an anchor at or below an ordinal the client already
  holds. Sync therefore selects control events with
  `after_ordinal >= since_ordinal` (note `>=`, not `>`): rows anchored
  exactly at the cursor are re-delivered and the client dedups them by their
  stable row id (`n<seq>`), which the DTO must carry. This keeps the wire
  cursor a single i64 instead of a compound `(ordinal, seq)` watermark.
  Normative implementation constraint: the message scan that fixes
  `next_cursor` MUST run before — or atomically with, in one read
  transaction — the control-event scan. Otherwise a control event written
  between the two scans, anchored at the pre-scan max ordinal, is missed by
  this sync AND excluded by every later `>=` select once `next_cursor`
  moves past its anchor — lost permanently.
- **Decided 2026-07-06 — `limit` election is per call site, not a global
  threshold.** Server hard cap stays 200 (today's `MAX_CATCHUP_REPLAY` /
  `CATCH_UP_LIMIT`). Clients pass `limit = 200` when merging into an
  already-rendered thread (a rebase is a REPLACE — discarding loaded older
  history and the scroll position under a reading user — so incremental
  merge is preferred all the way to the cap) and `limit = 50`
  (`HISTORY_PAGE_LIMIT`, one UI page) for baseline/cold opens, where
  `since = null` is a newest-page REPLACE by definition and rebase semantics
  cost nothing. Both numbers are existing constants; no new tunable.
- Gap > `limit` → **rebase**: respond with the newest page and
  `rebased: true` instead of an empty `truncated` result. The client REPLACEs
  its thread with the page and keeps `oldest_ordinal`/`has_more_older` for
  lazy backfill — Telegram's `channelDifferenceTooLong` shape (which returns
  the *most recent* messages, not the requested range) rather than the current
  catch-up's "here's nothing, go escalate yourself".
- Rows carry work blocks and notices on **every** path. The server-side
  reconstruction already exists (`reconstruct_transcript_with_attachments`,
  the in-flight fold on the newest page, control-event interleave —
  `chat.rs:790-833`); sync reuses it wholesale. Work items for turns
  straddling the page boundary reconstruct partially until an older page
  loads — the same accepted partiality `get_session` has today.
- Transport: plain REST on both legs (direct HTTPS; relay Noise API tunnel) —
  identical to how catch-up/history travel today. No new WS frame is needed
  for the pull side; "pull as truth" over a short connection is exactly the
  weak-network posture the industry pattern prescribes.

Backward paging is unchanged in role: `GET /v1/chat/sessions/{id}` with
`before_ordinal` stays as the `/messages`-style lazy backfill, but returns the
same `TranscriptRow` DTO and **must not be filtered client-side** (the iOS FFI
`kind == "message"` filter is deleted; negative-ordinal control rows get their
own row keys instead of polluting the ordinal dedup space).

### State plane: subscribe returns a bundle, not a history

`Subscribe` loses its replay half entirely. `since_ordinal` is removed from
the frame outright (no compatibility shim — see Migration). In exchange the
server sends **one**
snapshot frame immediately after registering the subscription:

```
Frame::SubscribeState {
  session_id,
  as_of_ordinal: Option<i64>,         // session's newest persisted ordinal
                                      // at snapshot time
  turn: { active: bool, started_at: Option<DateTime> },
  work_steps: Vec<WireWorkStep>,      // empty unless mid-turn
  pending_approvals: Vec<ApprovalCard>,
  tasks: Vec<TaskView>,
}
```

This replaces the current four-frame drizzle (`TurnState`, `WorkSnapshot`,
`PendingApprovalsSnapshot`, `TaskList` — `route.rs:294-481`) with a single
atomic REPLACE. `pending_approvals` is the **authoritative replacement set**
— full cards, built from one atomic queue read — which closes the documented
approval-snapshot race window (`wire/src/lib.rs:666-671`); a live
`ApprovalRequested`/`ApprovalResolved` arriving after the snapshot wins over
it (normal frame order on one connection). `as_of_ordinal` stamps the
snapshot's transcript position, but the normative staleness test for the
`turn`/`work_steps` halves is **turn identity, not ordinal arithmetic** —
the coverage watermark advances continuously during an active turn (tool
rows persist mid-turn), so `cursor > as_of_ordinal` does NOT imply the turn
ended, and an arithmetic discard would wipe the only mid-turn state source
for a joining client. The rule: discard the `turn`/`work_steps` halves only
when the client already holds a turn-end signal for the SAME turn, matched
by `started_at` — a `TurnState { active: false }`, the turn's final
assistant `Message`, or its closed work row from sync. Approvals and tasks
need no such test: they are latest-wins REPLACEs, and later live frames win
by arrival order. Live updates to each component keep their existing frames
and REPLACE/append semantics.

**Decided 2026-07-06 — the bundle stays a subscribe-time frame, NOT part of
the sync response.** Sync must remain a stateless store read answerable over
any leg and any short connection (the weak-network posture rests on that);
state has its own live-update frames, so the 3-minute safety tick would haul
dead snapshots; and `as_of_ordinal` already orders the snapshot against sync
pages — the one seam coupling would have fixed. Semantically: state is "what
you need to know the moment you start listening" (subscribe), history is
"what happened since you left" (sync). Two questions, two answers.

### Live plane: server-declared loss; the assistant final is ordinal-stamped

- The final assistant `Message` already carries its ordinal
  (`adapter.rs:464-467`). The **user echo stays exactly as it is**:
  pre-persist, `ordinal: None` (`route.rs:511-513`, `adapter.rs:362`).
  **Decided 2026-07-06** — moving the echo after persistence was considered
  and rejected: user rows are persisted by the actor at turn *start*
  (`handle_merged_user_turn`, `crates/agent/src/actor/mod.rs:610-630`), so a
  message sent while a turn is in flight would not echo to other clients
  until the running turn finished — an unacceptable multi-client latency
  cliff — and hoisting persistence out of the actor would break the
  actor-owns-session-writes invariant that makes lock-free ordinal
  assignment sound. The consequence is accepted instead: **cursor
  completeness is not a protocol requirement.** A client's cursor lags
  just-sent user rows until the assistant reply or the next sync advances
  it; sync then re-delivers those rows and the client dedups by
  `platform_msg_id`, which the echo and every replay/sync row all carry
  (`route.rs:812` — the web comment claiming replay zeros it is wrong, and
  the text-match heuristics it justified can be deleted). WeChat's SyncKey
  model makes the same trade: cursors may lag; redelivery + idempotent
  apply converge.
- **Gap detection is server-declared, not client-arithmetic.** Telegram needs
  `local_pts + pts_count` arithmetic because its transport can drop updates
  silently. baybo's WS is ordered and reliable; the only in-connection loss
  point is the server's own slow-consumer drop (`channel.rs:396-401`) — and
  the server *knows* when it dropped. So: replace the connection-level
  `Frame::Reset` with a per-session nudge —

  ```
  Frame::Gap { session_id: Option<SessionId> }
  ```

  `Some(id)` means "I dropped transcript frames for this session; run sync
  for it". Web stops wiping nineteen innocent session views; iOS stops
  rebuilding at degraded fidelity (sync is full-fidelity by construction).
  `None` covers loss the server cannot attribute to one session — dropped
  session-less broadcasts (`SessionActivity`, `SessionUpdated`,
  `FoldersChanged` ride the same bounded per-connection queue,
  `channel.rs:396-401`) — and means "resync every subscribed session and
  refetch the session list + folders". The `Gap` frame itself rides the same
  best-effort queue it reports on; the safety-net pull below is the backstop
  for a lost `Gap`. `Frame::Reset` is retired.
- Ephemeral frames (`AnswerDelta`, `Reasoning`, `ToolStarted/Completed`,
  transient `Notice`, `Status`) are unchanged: droppable, never replayed,
  outcome recoverable via `SubscribeState.work_steps` mid-turn or the
  reconstructed work row after the turn.

### Signal plane (unchanged in role, one scoping fix)

- `SessionActivity` (SessionPulse) and the APNs path stay exactly as they
  are: fire-and-forget accelerators for the list UI. Unread stays
  client-local; server-side read cursors are explicitly **out of scope** (a
  future feature, not a sync-protocol concern).
- One hygiene fix rides along: `SessionUpdated` / `FoldersChanged` broadcasts
  are currently fanned to both channels unconditionally
  (`chat_broadcast_channels() = [http, device]`, `chat.rs:1682-1699`), which
  plants clickable ghost rows for device sessions in web sidebars (web's
  `applySessionPatch` constructs rows for unknown ids, `ChatPage.tsx:3727-3739`),
  and the WS `Subscribe` handler does no channel-vs-session scoping
  (`route.rs:294-309`). Scope the patch broadcast to `session.channel`, and
  reject `Subscribe` for sessions outside the connection's channel — the WS
  layer then enforces the same universe boundary the REST layer already does
  (`load_scoped_chat_session`, `chat.rs:1658-1675`).

### The one client algorithm

Both clients run this loop; there are no other hydration paths:

```
on open / reconnect / foreground / Gap / push-tap / safety timer:
  subscribe(session)                  # → SubscribeState snapshot; live frames
                                      #   start arriving — buffer them
  page = sync(since = local_cursor)   # local_cursor may be null
  if page.rebased or local_cursor == null:
      REPLACE thread with page.rows   # newest-page baseline
  else:
      APPEND/merge page.rows          # dedup by ordinal / platform_msg_id
  cursor = page.next_cursor ?? cursor
  apply buffered frames (ordinal/platform_msg_id dedup), then go live
```

On reconnect and on `Gap(None)`, clients additionally refetch the session
list and folders — the list/folder plane has no cursor, so pull-on-reconnect
is its only loss recovery (web's current one-shot bootstrap gains a reconnect
refetch).

Consequences per client:

- **iOS.** The mirror becomes a pure cache: `{rows, cursor}` written
  atomically exactly as today (`ChatStore.swift:50-55` — that invariant
  stays). Deleted outright: the `listed` flag, `ensureBackfilled` and both of
  its clock edges, the seven-cell hydration matrix, the `needsHistoryReset`
  special case (buffer overflow just clears the buffer and runs the loop),
  and all three `?? 0` sentinel sites (the null/`-1`/`0` tri-state collapses
  — `null` is the only "no cursor" and it means *newest page*, which cannot
  skip row 0). The FFI keeps two calls: `sync` and `history_page(before)`,
  both passing rows through unfiltered.
- **Web.** Gains the sync call it never had; the reconnect path becomes
  subscribe → sync per subscribed session instead of relying on WS replay.
  The `-1` sentinel, the replay-reconciliation heuristics for gap rows, and
  the wipe-the-world Reset handler all go. (The text-match heuristics were
  only ever justified by the wrong comment claiming the gateway zeros
  `platform_msg_id` on replay — `route.rs:812` preserves it, so
  `platform_msg_id` equality replaces them outright.)
- **Safety-net pull** (RongCloud's 3–5 min re-pull): **default-on**.
  **Decided 2026-07-06:** every 3 minutes, for the FOREGROUND visible
  session only, and the tick is skipped when **any frame for that session**
  arrived within the last interval (one timestamp compare) — ephemeral
  frames included: deltas prove liveness just as well, and since the only
  ordinal-stamped live frame is the final reply, an ordinal-only predicate
  would fail the skip on every long streaming turn, the exact case the
  exemption exists for. Background subscribed sessions are
  deliberately excluded — their transcripts heal on the open edge anyway,
  and list recency/unread ride `SessionActivity` + the REST list merge, so
  polling them buys nothing user-visible while costing mobile radio wakes.
  The `Gap` nudge covers the *known* loss point; this timer is the backstop
  for a lost `Gap` and for suspended-client windows.

### Send path (outbox)

Unchanged wire contract (`platform_msg_id` idempotency + server `InboundDedup`
+ echo-as-ack), upgraded client discipline to the industry state machine:

1. Message enters a **persisted outbox** in state `sending`, keyed by
   `platform_msg_id` — iOS: a file alongside the mirror (survives relaunch);
   web: `localStorage` under `baybo.outbox.<sessionId>` (survives tab close;
   same pattern as the existing `baybo.queue.<sessionId>` store).
2. **Confirmation is two-stage**, because the echo is pre-persist and proves
   transport, not durability — a mid-turn send sits unpersisted in the actor
   mailbox until the next turn boundary (`actor/mod.rs:610-655`). The echo
   flips the UI `sending → sent` and stops in-connection retries; the outbox
   entry is **retained** until an ordinal-stamped row with the same
   `platform_msg_id` is observed — from sync, replay, or a backfill page
   (all three confirm durability and release the entry). **Sync is the
   reconciler**: a send whose ack was lost to the network is confirmed by
   the next sync instead of being re-sent or stuck red (the Tencent-IM
   "pull overwrites failed" rule).
3. **Decided 2026-07-06 — the retry machine** (mechanics amended by the
   same-day verification pass): the in-connection resend fires when **no
   echo** arrives within **10 s** (transport loss; blind resend is safe
   inside the live dedup window) — durability lag alone never triggers it,
   since a mid-turn persist legitimately takes minutes. On every reconnect
   edge the loop's sync runs first — that *is* the reconciliation gate —
   then every entry still lacking *durability* confirmation resends; the
   gateway-restart crash window (echoed, never persisted, mailbox lost) is
   exactly what this recovers. Hard cap: **3 automatic transmissions** per
   message (initial + 2 retries), then `failed` + the manual retry
   affordance (manual retry reuses the same msgId).
4. **A rebased sync makes "absent" unknowable from the page alone**: a
   `rebased: true` response shows only rows above the new floor, so a
   pre-floor outbox entry must be neither resent (double-run if the dedup
   key aged out of the 4096-FIFO) nor failed (its row may sit below the
   floor) on that evidence. Such entries go `unknown` and are resolved —
   without consuming a transmission — by a `platform_msg_id` point lookup
   (`GET /v1/chat/sessions/{id}/messages?platform_msg_id=…`, served from
   the already-persisted column): found → confirmed; provably absent → the
   normal retry machine resumes.

Accepted loss window, documented: `/stop` deliberately discards queued
interjections (`actor/mod.rs:657-664`). Their echoes already fired, their
rows never persist, and resends are swallowed by the live dedup — so they
exhaust the cap and surface as `failed`, which is honest feedback ("this
message was not processed"). A future `Dropped { platform_msg_ids }` control
event could surface it immediately; out of scope for v2. Honest guarantee: the gateway's `InboundDedup` is an in-memory
   FIFO of 4096 keys (`dedup.rs:27`), *not* Telegram's never-expiring
   `random_id` store — blind retry is safe within a live gateway process and
   the recent-send window, but a retry racing a gateway restart can
   double-run a turn. Retries fired after a reconnect (or any suspected
   server restart) MUST therefore be gated on sync reconciliation first
   (does a row with my `platform_msg_id` exist?). If exactly-once ever
   matters more, the server-side option is a durable idempotency check —
   rows already persist `platform_msg_id`, so a store lookup on dedup miss
   closes the window; deliberately out of scope for v2.

This replaces today's "one shot, red dot, human retries" on both clients
(`ChatStore.swift:360-375`, and web's send-requires-connected guard).

## What this deletes

| Today | Fate |
|---|---|
| WS `Subscribe` replay + `since_ordinal` field | deleted outright, field removed from the wire (the TUI always sent `None` — it never replayed) |
| REST `catch-up` endpoint | absorbed into `sync` (it is `sync` minus notices minus rebase), then deleted |
| `Frame::Reset` | replaced by `Frame::Gap { session_id: Option }` + `rebased` in sync; wire variant deleted |
| `TurnState`/`WorkSnapshot`/`PendingApprovalsSnapshot`/`TaskList` on-subscribe drizzle | one `Frame::SubscribeState`; `WorkSnapshot` + `PendingApprovalsSnapshot` wire variants deleted (on-subscribe was their only role); `TurnState`/`TaskList` keep live-update roles only |
| iOS FFI `kind == "message"` filter | deleted; rows pass through on every path |
| `listed` flag, `ensureBackfilled`, hydration matrix cells A–G, `needsHistoryReset` | deleted — `sync(null)` is the only cold path |
| `None` / `-1` / `0` cursor tri-state | `null` = baseline, `n` = difference; nothing else |
| web Reset wipe-all + replay heuristics | deleted |
| audit holes 1–4 above | structurally impossible, not patched |

Explicit non-goals: cross-channel session sharing (the `http`/`device`
universes stay disjoint — this proposal *hardens* the boundary, see the
Subscribe scoping fix); server-side read cursors / cross-device unread (future
feature); Signal-style per-device mailboxes (baybo's server holds plaintext
transcripts, so pull-from-truth is strictly better than ack-and-delete
queues); changing blob transfer or pairing.

## Migration — clean cut, no compatibility

**Decided 2026-07-06 (owner):** there is NO backward-compatibility
requirement. The fleet is single-operator — one gateway, one phone, in-repo
web/TUI — so legacy sync code is **deleted outright at cut-over**, not kept
behind gates. An old app build simply stops working until updated;
accepted. The capability-gate mechanism proposed by the verification pass
(`caps: ["sync-v2"]` on `Register`, per-connection legacy suppression,
sunset windows) is dropped as unnecessary machinery — it solved a
fleet-adoption problem this project does not have.

Cut-over shape (two artifacts, one seam):

1. **One atomic in-repo change series** — server, web, and TUI live in the
   same workspace and ship together, so old and new never coexist there.
   Add: `sync` (reusing `get_session`'s reconstruction internals), the
   `platform_msg_id` point lookup (outbox rule 4), `Frame::SubscribeState`,
   `Frame::Gap`, the two scoping fixes (patch broadcast + Subscribe channel
   check). The echo is untouched (decision 1 — no server work exists for
   it). Delete, in the same series: the `Subscribe.since_ordinal` replay
   arm AND the field itself (the TUI always sent `None` —
   `crates/tui/src/client/ws.rs:169-178` — so nothing breaks), the REST
   catch-up endpoint, `Frame::Reset` (emission and wire variant), the
   on-subscribe snapshot drizzle — the `WorkSnapshot` and
   `PendingApprovalsSnapshot` wire variants disappear entirely (their only
   role was on-subscribe; `SubscribeState` carries both), while `TurnState`
   and `TaskList` keep their live-update roles and just stop being sent on
   subscribe — plus web's Reset wipe-all handler, the replay-reconciliation
   heuristics, and the `-1` sentinel. The TUI's `HistorySnapshot` (input
   history, unrelated to transcript sync) is untouched. ts-rs + OpenAPI
   regen (`scripts/check-ts-bindings.sh`).
2. **iOS app build in lockstep**: FFI swaps `fetch_catch_up` / the filtered
   `fetch_history_page` for `fetch_sync` / unfiltered paging; Swift deletes
   the backfill/reset/hydration-matrix machinery; the webview renders
   work/notice rows on every path (renderers already exist for the
   catch-up and live paths); the outbox lands. Include the relay codec
   skip-unknown decode fix (`app/ios/ffi/src/core/content.rs:54-59`, to
   match `direct/chat.rs:33-38`) — no longer load-bearing for THIS cut
   (old builds are allowed to die), but it is cheap hygiene that makes the
   *next* protocol addition a non-event.
3. **Docs sweep**: update `docs/web-chat.md`, `docs/turn-progress-events.md`
   (the "web recovers via reload" over-claim), `docs/modules/gateway.md`;
   delete the hydration matrix from `app/ios/CLAUDE.md`.

The only ordering constraint left is trivial and human-scale: install the
new app build around the time the gateway restarts on the new protocol. An
old build against the new server dies on its first unknown frame over relay
(decode-fatal) and finds `catch-up` gone over REST — expected, and fixed by
updating the app.

## Decision log

The five launch open questions were all resolved on 2026-07-06 (each decision
is also recorded inline, marked **Decided 2026-07-06**, in its own section):

1. **Echo ordinal** — the user echo stays pre-persist / ordinal-less; cursor
   completeness is explicitly not a protocol requirement (sync redelivery +
   `platform_msg_id` dedup converge). Post-persist echo was rejected because
   user rows persist at turn *start*, so a mid-turn send would not echo to
   other clients until the running turn finished.
2. **Safety-net pull** — default-on, every 3 minutes, foreground visible
   session only, tick skipped when the stream was live within the interval.
3. **Rebase threshold** — per call site, not global: `limit = 200` when
   merging into a rendered thread, `limit = 50` for baseline/cold opens;
   server hard cap stays 200.
4. **Outbox** — persisted on both clients; two-stage confirmation (echo =
   transport/UI, ordinal-stamped row = durability/release); one 10 s
   no-echo resend, resend-after-sync on every reconnect edge, 3 automatic
   transmissions total, then `failed` + manual retry; rebase-floor entries
   resolve via the `platform_msg_id` point lookup.
5. **`SubscribeState` stays separate from `sync`** — sync remains a
   stateless store read; state rides its own frames; the turn-identity rule
   orders the seam.

A same-day adversarial verification pass (consistency checker + protocol
skeptic) then amended the *mechanics* without touching any decision:
two-stage outbox confirmation + the rebase point lookup, the rebase-dirty
cursor and REPLACE-overlay rules, turn-identity staleness for
`SubscribeState`, emitted-row rebase counting, the control-event scan-order
constraint, and a capability-gated migration.

6. **No backward compatibility (owner, same day, supersedes the
   capability-gate amendment)** — legacy sync code is deleted outright at
   cut-over; old app builds break until updated (single-operator fleet,
   accepted). See "Migration — clean cut, no compatibility". This also
   deletes wire surface the gated plan would have kept: the
   `Subscribe.since_ordinal` field, `Frame::Reset`, `Frame::WorkSnapshot`,
   and `Frame::PendingApprovalsSnapshot` all leave the protocol.
