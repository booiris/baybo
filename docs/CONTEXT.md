# Baybo Chat Sync

The vocabulary of chat transcript synchronization between the gateway and its
chat clients (web, iOS, TUI), as settled in
[`sync-protocol.md`](sync-protocol.md) (designed 2026-07-06; **built
2026-07-06** — the v1 mechanisms are deleted). Terms below are the canonical
names; the "avoid" aliases are v1 mechanism names the migration retired —
using them now is a smell.

## Language

**Ordinal**:
The per-session monotonic sequence number a transcript row receives when the
actor persists it; the unit of transcript-row sync addressing.
_Avoid_: seq (as a name for the Ordinal — **Control events** keep their own
live `seq`), index, position

**Cursor**:
The highest ordinal a client knows it has covered for one session; `null`
means "no baseline yet", never a sentinel number.
_Avoid_: sync key, since-value, `-1`/`0` sentinels

**Sync**:
The one forward-recovery pull call that returns full-fidelity transcript
rows after a cursor (or the newest page when the cursor is `null`).
_Avoid_: catch-up, replay, hydration (all name retired v1 mechanisms)

**Coverage watermark**:
Sync's `next_cursor` — the highest persisted ordinal the server's scan
covered, visible or not; it may exceed every row in the page, and it (not
row ordinals) is what advances the Cursor.
_Avoid_: last row's ordinal, max row ordinal

**Baseline**:
The newest-page state a client adopts when it has no cursor or after a
rebase; older history stays fetchable.
_Avoid_: initial sync, cold start fetch

**Rebase**:
The server answering sync with a fresh baseline instead of the difference
because the difference would exceed the requested limit — counted in
emitted rows, not ordinal distance (hitting the server's scan bound also
rebases).
_Avoid_: reset, truncated

**Rebase-dirty**:
The cursor state after applying a rebased page — live ordinals render but
do not advance the cursor; only a Sync `next_cursor` does, until one
non-rebased Sync completes.

**Backfill**:
Pulling older rows (`before_ordinal`) to extend a thread upward on scroll.
_Avoid_: paging (ambiguous), and the retired iOS "initial backfill"

**Gap nudge**:
The server telling a connection it dropped frames (for one session, or
`None` for unattributable loss); the client's response is Sync — on `None`,
Sync every subscribed session plus a refetch of the session list and
folders (that plane has no cursor; the refetch is its only recovery).
_Avoid_: reset

**Echo**:
The server's immediate pre-persist re-broadcast of an inbound user message
to the session's subscribers; ordinal-less by decision, and doubles as the
transport-level send acknowledgment (durability is confirmed separately, by
an ordinal-stamped row).
_Avoid_: ack (alone), user reflect

**platform_msg_id**:
The client-generated idempotency key stamped on a send; the Echo and every
redelivery of that row (Sync, Backfill) carry it, so it keys the Outbox,
matches confirmations to entries, and dedups redelivered rows.
_Avoid_: msgId (bare), random_id, txnId

**Outbox**:
The client-persisted queue of sends not yet durability-confirmed (an Echo
alone never releases an entry), keyed by `platform_msg_id`. Entry states:
`sending` → `sent` on Echo; released by an ordinal-stamped row; `failed`
after 3 automatic transmissions; `unknown` when a Rebase hides the floor
(resolved by the **Point lookup**).
_Avoid_: pending queue, retry queue

**Point lookup**:
The per-`platform_msg_id` durability probe that resolves a rebase-floor
Outbox entry (state `unknown`) without consuming a retry transmission:
found → confirmed; provably absent → the retry machine resumes.
_Avoid_: resend-on-rebase, fail-on-rebase

**Safety-net pull**:
The periodic foreground Sync tick that backstops lost Gap nudges and
suspended-client windows.
_Also_: "safety tick" / "safety timer" (the protocol doc's shorthand)
_Avoid_: polling (it is not the primary transport)

**Durable plane**:
Persisted transcript rows (message / work / notice), never lossy; message
and work rows are ordinal-addressed, notice rows are **Control events**.

**Control event**:
A notice/slash-echo row — NOT ordinal-addressed: keyed by its own
per-session `seq`, anchored to an ordinal (`after_ordinal`). Sync selects
them at `>=` the cursor (anchored-at-cursor rows re-deliver) and clients
dedup by stable row id (`n<seq>`), never by ordinal.
_Avoid_: notice ordinal, negative-ordinal rows

**State plane**:
Idempotent REPLACE snapshots of current session state (turn, work steps,
approvals, tasks) — delivered as one **SubscribeState** bundle on
subscribe, repaired by the next snapshot, not by history.

**SubscribeState**:
The one atomic state-plane bundle (turn, work steps, pending approvals,
tasks, `as_of_ordinal`) the server sends immediately on subscribe; by
decision a subscribe-time frame, never part of Sync. Its turn/work halves
go stale by **Turn identity**, not ordinal arithmetic.
_Avoid_: on-subscribe drizzle (the retired four-frame
`TurnState`/`WorkSnapshot`/`PendingApprovalsSnapshot`/`TaskList` sequence)

**Turn identity**:
The staleness test for SubscribeState's turn/work-steps halves: discard
them only when the client already holds a turn-end signal for the *same*
turn, matched by `started_at` — never by comparing the cursor to the
snapshot's `as_of_ordinal` (the cursor advances mid-turn).
_Avoid_: ordinal-arithmetic staleness (`cursor > as_of_ordinal`)

**Ephemeral plane**:
Droppable live streams (answer deltas, reasoning, tool lifecycle, activity
pings); their durable *outcome* lives in the other two planes.

**Mirror**:
iOS's on-device cache of rendered rows plus the cursor, written atomically;
a pure cache — never a source of truth.
_Avoid_: local store, offline database

**Channel universe**:
The disjoint session sets owned by the `http` (web) and `device` (iOS)
channels; sessions never cross between them.

## Relationships

- A **Session** owns one ordinal sequence; every message and work row has
  exactly one **Ordinal**. A **Control event** carries none: it anchors at
  an ordinal, is re-delivered by the `>=` scan, and dedups by its row id.
- A client holds one **Cursor** per session, advanced max-wins by Sync's
  **Coverage watermark** (`next_cursor`, not the last row's ordinal) and by
  ordinal-stamped live frames — except while **Rebase-dirty**, when only a
  Sync `next_cursor` advances it.
- A **Rebase** REPLACEs the thread with a **Baseline**; **Backfill** extends
  it older; the two never interleave in one call.
- An **Echo** confirms *transport* for an **Outbox** entry; an
  ordinal-stamped **Sync**/**Backfill** row confirms *durability* and
  releases it (same `platform_msg_id`). An entry below a **Rebase** floor
  goes `unknown` and is resolved by the **Point lookup** instead.
- A **Gap nudge** and the **Safety-net pull** both terminate in **Sync** —
  the only transcript-recovery verb; `Gap(None)` additionally refetches the
  session list and folders (no cursor on that plane, so pull is its only
  recovery).
- A **SubscribeState** arrives on subscribe, before the first **Sync**
  page; `as_of_ordinal` orders the two, but its turn/work halves are
  discarded only by **Turn identity**.

## Example dialogue

> **Dev:** "After a reconnect, do we replay the missed frames?"
> **Domain expert:** "No — the server replays nothing. The client Syncs
> from its Cursor; if the gap is too wide the server Rebases it onto a
> fresh Baseline and the older rows come back later via Backfill. Live
> frames the client buffered during the Sync are applied after, deduped by
> ordinal — or by `platform_msg_id` for the ordinal-less Echo."
> **Dev:** "And if the send's Echo never arrives?"
> **Domain expert:** "No echo within 10 s → one blind in-connection resend,
> safe inside the live dedup window. Across a reconnect edge, Sync runs
> first — that is the reconciliation gate — then any entry still lacking
> durability confirmation resends. Three transmissions total, then `failed`
> with a manual retry."

## Flagged ambiguities

- "**catch-up**" historically named both the WS Subscribe replay and the
  REST endpoint — both retired at cut-over; the concept is just **Sync**.
- "**backfill**" historically meant iOS's `ensureBackfilled` initial
  hydration — that mechanism was deleted at cut-over; the word now means
  backward paging only.
- "**reset**" conflated three things (over-cap replay, slow-consumer drop,
  client rebuild) — split into **Rebase** (server answer) and **Gap nudge**
  (server notice); the client-side transcript verb is always **Sync**.
- "**replay**" named the retired WS `Subscribe { since_ordinal }` mechanism
  — that is gone; the server replays nothing. The v2 client loop still says
  "replay buffered frames" for applying frames the client itself buffered
  during a Sync — prefer "apply buffered frames" for that step.
- "**plane**" is overloaded in the protocol doc: the data model has exactly
  the three planes above, but section headings also use it for delivery
  surfaces ("Live plane", "Signal plane"). The Signal plane is not a fourth
  data plane — it names the fire-and-forget accelerator role for the list
  UI (`SessionActivity`, APNs); its frames are Ephemeral-plane data.
- "**snapshot**" always means a State-plane REPLACE; the retired
  `WorkSnapshot` / `PendingApprovalsSnapshot` frames were its old per-frame
  on-subscribe delivery (now the single **SubscribeState**). The TUI's
  surviving `Frame::HistorySnapshot` (input history) is unrelated to
  transcript sync.
