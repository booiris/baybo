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
  page = sync(since = cursor)          # cursor null → newest-page baseline
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

On the reconnect edge the sync runs first (the reconciliation gate), then
unconfirmed entries resend. A **rebased** sync hides the floor, so each
unconfirmed entry goes `unknown` and resolves via the per-key point lookup
(`chatLookupMessage`) — found → released, absent → retry resumes.
