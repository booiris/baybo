# Transcript sync, mirror retention, and the send outbox

*How the iOS chat transcript loads and recovers rows (sync-protocol v2), why the on-disk transcript mirror is never swept, and how sends survive a flaky leg — governs `app/ios/App/Core/ChatStore.swift`, `app/ios/App/Core/OutboxStore.swift`, the transcript mirror store, and `app/ios/web/src/Transcript.tsx` + `app/ios/web/src/bridge.ts`.*

## Send path

Native mints the msgId, seeds the webview's optimistic bubble + echo-dedup FIRST,
enrols the persisted outbox entry, then enqueues on the leg (see "Send outbox"
below).

## Transcript sync (sync-protocol v2)

> **Read `docs/sync-protocol.md` BEFORE touching sync or lifecycle.** This
> section is a client-side companion to that protocol doc, not a replacement for
> it.

The old seven-cell hydration matrix is **retired**. Transcript loading and
forward recovery are now **one loop** (`docs/sync-protocol.md`), identical on
both legs (same web bundle, same `GatewayJsonClient` API surface):

```
on open / reconnect / gap / buffer-overflow re-attach / safety tick:
  page = sync(since = syncSince(cursor, rows))   # null → newest-page baseline
  if page.rebased or cursor == null:
      REPLACE thread with page.rows    # keep the open work block + optimistic sends
  else:
      APPEND/merge page.rows           # dedup by row id / platform_msg_id
  cursor = max(cursor, page.next_cursor)   # frozen while rebase-dirty
```

### The webview drives sync

`app/ios/web/src/Transcript.tsx`'s `runSync()` posts
`{type:"sync", sinceOrdinal, limit}` over the bridge; native
(`ChatStore.requestSync`) fetches `GET …/sync` over the active leg and pushes a
synthesized `sync_page` frame back.

Clock edges — there are five:

- mount (resident re-entry),
- the `connEpoch` bump (`handleConnEpoch` — reconnect),
- a `gap` frame,
- the offscreen-buffer-overflow re-attach (native `bridge.requestSync()`),
- the 3-minute safety tick.

`syncInFlight` coalesces a burst to one pull.

### A difference must EXTEND the thread, never overlap it

`syncSince` (`Transcript.tsx`) is the gate, and it exists because of a shipped
bug. The cursor is a **coverage watermark**, not a high-water mark of what is
rendered: scroll-up paging (`prependOlder`) and the rebase-dirty freeze both put
rows on screen without advancing it. Ask for `since = cursor` from such a thread
and the server answers correctly — a *difference*, so no `rebased` REPLACE ever
fires — while the merge, which appends any row it doesn't already hold, welds
that whole span onto the BOTTOM of the thread and fuses the page's leading work
block into the tail's card. One device rendered three hours of conversation 75
rows too high, under a single "Worked 2h 47m" block; nothing was lost, and
nothing could self-heal (a re-sync returns nothing, and `sanitizeRestoredRows`
does not sort).

So: if any rendered row carries a durable ordinal **above** the cursor, present
`null` and take the baseline REPLACE — the path a fresh install proves correct.
"Durable ordinal" is `rowCoverageOrdinal`, not the id alone: a user row is KEYED
by its `platform_msg_id` (so an optimistic bubble reconciles), so it carries the
server `ordinal` as a field beside the id. Reading only the id made the gate
blind to every user message ever sent — from this device or another — which is
precisely the rebase-dirty case it exists for. `app/web` never had the hole: it
keys every row `row-<sid>-m<ordinal>` and keeps `clientMsgId` separately.
**This prevents the scramble; it does not repair one.** The welding sync ends by
advancing the cursor to `next_cursor`, and a difference is only returned when its
scan did not overrun, so that watermark is the session's newest ordinal — above
every rendered row. A mirror already scarred is persisted covering itself, so the
gate stays quiet and the order stands; the exit is the per-session resync. It is quiet
on a healthy thread (a sync's `next_cursor` covers every row it delivered, a
live final reply advances the cursor with itself, and paged rows are older than
everything) and it is self-terminating (a REPLACE leaves the thread a prefix of
its own watermark). Ordinal-less rows — an optimistic send, a live work block —
are not durable coverage and never trip it; a send gains its ordinal when the
echo (`markSent`) or its durable twin (`mergeSyncPage`) confirms it.

### Only the server says two work blocks are one turn

Two adjacent `work` rows used to be folded into one card on ADJACENCY alone
("a healthy turn has a message row between its block and the next"). The
scramble above disproved the premise — three turns' blocks ended up side by side
and welded into a single "Worked 2h 47m" card — and it is not even bug-specific:
a turn whose empty final reply leaves no bubble abuts the next fire the same
way.

So the fold asks the server instead. Every reconstructed `work` row carries
`turn_complete` (`ChatTranscriptItem`, already on the wire — the FFI passes sync
/ history rows through verbatim): `false` = the page window's trailing edge cut
this block off and its turn continues into the adjacent page, `true` = a real
boundary (the final answer, the next user turn, a `/stop`, a compaction
watermark) closed it in-window. `sameContinuingTurn` (`Transcript.tsx`, mirroring
`app/web`'s) lets a block be JOINED onto the previous one only when that previous
one is a cut-off (`false`) head — every seam asks it: `foldAdjacentWork` (the
prepend and REPLACE folds), `mergeSyncPage`'s tail fold, and the REPLACE
overlay's live-block fuse. A live block is keyed by `uid()`, carries no flag, and
still fuses with its own reconstruction — that is RECONCILE (one span, two
representations), not a join. An ABSENT flag declines: an extra card is
cosmetic, a wrong join swallows a whole turn into another's card.

The mirror's restore heal (`sanitizeRestoredRows`, which folds a legacy
[work][work] tear) skips a head the server called complete — otherwise the next
cold open would weld back exactly what the guard kept apart.

`crossesCompaction` STAYS alongside it. It is subsumed only for a watermark the
server saw: a split inside one reconstruction window flushes the pre-compaction
half `turn_complete: true`, which `sameContinuingTurn` refuses on its own. A
watermark falling in the GAP between two pages is straddled by no single window,
so neither page splits its own half and the head is an ordinary cut-off block —
there the compaction guard is the only refusal. `mergeSyncPage` carries both
guards for the same reason `foldAdjacentWork` does.

### The server replays NOTHING on Subscribe

It answers with one `SubscribeState` bundle (turn/work state) and live frames.
`Subscribe` lost its `since_ordinal` field; `Frame::Reset` / `WorkSnapshot` /
`PendingApprovalsSnapshot` are gone.

### The mirror stays a pure `{rows, cursor}` cache

Written atomically. It is never a source of truth — a mirror-less open just syncs
a baseline. The cursor is `number | null` (`null` = no baseline, never a
sentinel); it lives in the persisted mirror blob (`lastOrdinal`), advanced from
the sync coverage watermark and live final-reply ordinals, frozen while
**rebase-dirty**.

### Draft vs listed

A compose draft stays empty until its first send: `ChatStore.requestSync` skips
the fetch until the session exists remotely (`listed || remoteSessionEnsured`).
The webview no longer needs a `listed` flag — its loop runs the same on every
open.

### Backward paging (scroll-up)

Unchanged in role: `fetchHistory(before)` → `history_page` frame, full-fidelity
rows (message/work/notice) keyed by their stable server `id`, no client-side
filtering.

### Re-verify after touching any of this

- (a) logout → re-pair the same gateway → open an old session;
- (b) open an old (unpinned, outside top-10) session → back to list → re-enter;
- (c) kill the app → relaunch → open a session;
- (d) open A mid-stream → back → open B → back → A shows A (not B), B's mirror is
  not overwritten by A's late flush.

### The single reused webview changes WHEN, never HOW

**The single reused webview (`TranscriptHost`) changes WHEN the transcript is
(re)mounted, never the sync loop.** Returning to the SAME session reuses the LIVE
React tree; opening a DIFFERENT session remounts `<Transcript key={sessionId}>`
from that session's `restoredState`, then its mount effect runs one sync. A
jetsam silently reloads → `ready` re-fires → re-mounts and re-syncs.
Cross-session safety rests on `app/ios/web/src/bridge.ts` clearing
`buffer`/`blobPending`/`pendingPersist` on `init` and `persist` writes being
session-tagged.

## Transcript mirror retention (do NOT re-add a sweeper)

**The mirror is the entire cold-open story.** `ChatScreen.onAppear` calls
`retarget` BEFORE `connectIfNeeded`, `deliverInit` reads `transcripts/<id>.json`
straight off disk, and `<Transcript>` seeds its rows in the `useState`
initializer — so a cached conversation paints on the first commit with **zero
network**. Nothing in the paint path reads `connState`. If a chat opens blank and
fills in only once the leg connects, the mirror was **missing**, not gated.

So: **a mirror is kept for every session this device has rendered, and no
capacity sweeper evicts it.** `save()` writes the registry and nothing else.

### The three deletion paths

A mirror is removed only when its conversation genuinely goes away, and there are
exactly three such paths — all of them a user's deletion, none of them a sweep:

- `beginHide` — the user deleted the conversation here. Deletes the file BEFORE
  the row guard: a racing merge may already have dropped the row, and the delete
  must still land.
- `merge` — the row is absent from the server's list, i.e. the user deleted it on
  another client. That rebuild is where it leaves the list for good, so its
  transcript leaves with it (a targeted set difference against the surviving ids,
  skipping `pendingMutations`, which `beginHide`/`rollBackHide` own — **not** a
  directory sweep).
- `removeAll` — logout/rebind: the rows belong to the gateway we just left.

The one delete that is **not** a conversation going away is
`dropTranscriptMirror` — the per-session resync escape hatch
([transcript.md](transcript.md#per-session-resync-the-escape-hatch)). The row,
the outbox and the pending approvals all stay; only the local rendering is
discarded, so the cold-open path can rebuild it from the gateway. It is still
user-triggered and still per-session: nothing here sweeps.

### Never mint the orphan in the first place

**An empty thread with no cursor never writes a mirror at all** (the persist
effect in `app/ios/web/src/Transcript.tsx` bails). The transcript mounts for every
compose draft — including the throwaway one prewarmed at launch — each under a
fresh uuid that never becomes a chat-list row, so persisting them minted a file
per abandoned draft that nothing could ever reach again (no row → no delete → and
nothing sweeps). Don't create the orphan rather than sweep for it later.

### The `TranscriptStore.prune` post-mortem

This is a scar, so don't re-cut it. `save()` used to end with
`TranscriptStore.prune(keeping: <top 10 by lastActive>)` and every ingredient of
that was hostile:

- `save()` runs on essentially everything (every list merge — *including the one
  the pop back from a chat fires* — every activity ping, every badge clear);
- `rows` after a merge is the gateway's WHOLE session list ranked by the
  **server's** `lastActive`, so rows this device has never opened (and every
  individual cron fire — the list's cron-group collapse is render-time only)
  spent keep-slots on mirrors that did not exist;
- and reading a chat earns nothing (`touch` deliberately does not bump
  `lastActive` — "ordering means message activity, not visits" — and a merge
  would overwrite it anyway).

Net effect: any conversation outside the ten most recently **messaged** ones had
the mirror it wrote on exit deleted seconds later, and opened blank on **every**
re-entry, forever. An active `*/30` cron job burned all ten slots in about five
hours. `app/ios/Tests/SessionIndexMirrorTests.swift` pins this; the retention
tests fail against the old prune.

### The set on disk only grows, deliberately

Mirrors are text (`{rows, cursor, imageDims}` — never blob bytes) and exist only
for sessions actually read here, each dying with its conversation, so the set on
disk tracks the conversations the user still has. Like the blob cache it only
grows, deliberately: bounding it further wants a stated retention policy, not a
surprise sweep.

### What no cache can fix

A session this device has never rendered (started on web/TUI, a cron fire, a push
tap into a new session, the first open after a re-pair) has no mirror by
construction — its first rows MUST come off the wire. That open shows the
`.thread-loading` line (empty thread + sync in flight), whose 400ms CSS delay
keeps it invisible on every open that resolves instantly — a restored thread, and
a compose draft, whose synthesized empty page lands well inside the delay.

## Send outbox (sync-v2)

The one-shot "red dot, human retries" send is replaced by a **persisted outbox**
(`app/ios/App/Core/OutboxStore.swift`, a JSON file per session under
`Application Support/baybo/outbox/`, wiped with the mirrors on logout).

Entries are keyed by `platform_msg_id` with a two-stage confirmation:

- the server's Echo (ordinal-less user message, same key) proves transport
  (`sending` → `sent`, observed in `ChatStore.outboxObserveFrame`);
- an ordinal-stamped row with the same key (from a `sync_page`, scanned in
  `reconcileOutboxAfterSync` before the frame reaches the webview) proves
  durability and releases the entry.

No echo within 10 s → one blind resend, capped at 3 transmissions, then `failed` +
the manual red-dot retry (`resetForManualRetry`).

### The re-seed runs on every mount

An entry's optimistic BUBBLE lives in the webview's thread, and the gateway has
no row to bring a queued send back from — so whenever the thread a page mounts
with is older than the outbox, the user's message silently vanishes and a
`failed` one loses the red dot that is its only retry affordance (the retry
payload lives in the web row). `ChatStore.replayUnconfirmedSends` re-seeds the
surviving entries, oldest first, `sendFailed` for the failed ones.

It runs on **every** mount edge — the `ready` of any page load, and every
`retarget` attach — never behind a "a resync just happened" latch. The
[per-session resync](transcript.md#per-session-resync-the-escape-hatch) is only
the loudest way to lose the bubbles; a jetsam between a send and the mirror's
debounced write, a session whose mirror was never written, and a rebased page
that dropped the row all do it too, and a latch reachable only from the hatch
left every one of those permanent. (The resync itself does not touch the outbox
file, and seeds nothing of its own: reached from the list, the session it
rebuilds usually has no page yet.)

Running it always is safe because the re-seed is **idempotent, and the WEBVIEW is
what makes it so**. A user row is keyed by its `platform_msg_id`, so
`handleUserSent` asks `holdsUserSend` and returns before appending — before
`awaitingReply` too, which would otherwise raise the composer's stop button on a
send that failed days ago. A live send mints a fresh uuid and can never take that
exit. Native cannot make the call itself: it does not know whether the tree it is
seeding came up from a mirror that already carries the bubbles.

The one native-side rule the idempotency needs: `retarget` skips the re-seed
while the page is not `ready`. Those calls would only queue in the bridge's
`pending`, and the `ready` that flushes them re-seeds again — the same work
twice, arriving before React has committed the first pass and can recognise it.

On the reconnect edge the sync runs first (the reconciliation gate), then
unconfirmed entries resend. A **rebased** sync hides the floor, so each
unconfirmed entry goes `unknown` and resolves via the per-key point lookup
(`chatLookupMessage`) — found → released, absent → retry resumes.
