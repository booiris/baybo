# Chat list, unread, and push routing

*The device-local session registry behind the chat list, the live unread and approval marks that ride on it, the app-icon badge, and where a push tap lands. Governs `SessionIndex`, `SessionActivityHandler`, `ChatRowBody`, `app/ios/App/Core/BadgeCenter.swift`, `AppDelegate.registerForPush`, the NSE, and the FFI `SessionListSink` plumbing.*

## Chat list data

`SessionIndex` (`Application Support/baybo/sessions.json`) is the device-local
registry backing the list on BOTH legs.

Both direct and relay merge `chat_list_sessions()` over it on
appear/foreground/pull:

- **direct** uses REST `GET /v1/chat/sessions` with the stored Bearer plus
  `x-baybo-device-id`;
- **relay** uses the Noise-protected API tunnel.

### Merge rules

Remote wins for existence (a row missing remotely was hidden elsewhere).

In-flight local mutations (`pendingMutations`) and the `mutationEpoch` guard
beat a stale snapshot; otherwise server values win wholesale — a local row only
fills fields the server left nil (never overrides them).

### Transcript mirrors

Per-session transcript mirrors live in
`Application Support/baybo/transcripts/<id>.json`, one per session this device
has rendered, and **nothing sweeps them** — see "Transcript mirror retention" in
[sync-and-outbox.md](sync-and-outbox.md).

The legacy single-session UserDefaults keys (`ChatDefaults`) are migrated once
and retired.

### Row gestures

Trailing swipe archives / deletes, leading swipe pins — the three everyday verbs
(`ChatListScreen.chatRow`). **Long-press** adds the fourth, the per-session
resync ([transcript.md](transcript.md#per-session-resync-the-escape-hatch)), in a
`.contextMenu` rather than a fourth swipe button: it is reached once a year, and
a menu row has space to say what it does. It lives on a row because it needs
no conversation opened first and belongs beside the other session-level
operations; the header capsule's model panel carried it first and no longer does.

The long-press is one shared modifier, `resyncContextMenu`, and **every screen
that lists a conversation applies it** — this list, `CronGroupScreen`'s fires and
`ArchivedScreen` — because where a row is listed says nothing about whether its
transcript can drift. A new session-listing screen wires it in too.

## Live list unread

The gateway broadcasts a throttled `Frame::SessionActivity` (per-session ping,
no content) to EVERY connection on the `owner` channel — subscribed or not —
when a user send echoes or a session's turn completes (`SessionPulse`, installed
on the shared `owner` chat channel that both the web dashboard and this app
register as; TUI is excluded).

The FFI transport special-cases that frame in `dispatch_inbound_frame`, routing
it to a connection-global `SessionListSink` (set once via
`set_session_list_sink`) instead of the per-session `FrameSink` — so a session
the device never opened still updates the list.

`SessionActivityHandler` → `SessionIndex.noteActivity` bumps `SessionRow.unread`
and recency (persisted; ignored for the foreground session and unknown ids) as a
between-pulls accelerator — the badge is server-computed (`unreadCount` on the
list summary) and reconciled on every list merge, and the webview's `mark_read`
advances the server-side read cursor (`chat_mark_read`) so the badge clears
across devices.

`ChatScreen` enter/leave marks the foreground session and clears its badge.

Relay warms the leg via `relay_preconnect`; direct via `direct_preconnect` (both
best-effort on launch/foreground) so the pings arrive while parked on the list.

## Chat-list approval mark

A conversation whose tool call is parked on the gateway's approval gate wears an
ink `hand.raised` glyph in the row's trailing meta column, leading the unread
capsule (`ChatRowBody.approvalMark`); a cron GROUP row ORs it over its fires.

The bit is `SessionRow.approvalPending`, fed by two paths that mirror unread's:

- server-computed `approvalPending` on the REST list summary (cold-start truth);
- a connection-global `SessionUpdated{approval_pending}` patch tee'd to
  `SessionListSink` (`on_approval_pending` → `SessionIndex.noteApprovalPending`)
  as the between-pulls accelerator.

### Why a broadcast PATCH, not `Frame::ApprovalRequested`

It rides a broadcast PATCH rather than `Frame::ApprovalRequested` because that
frame only reaches connections **subscribed to that session**, and the client
that needs it is the one parked on the list, subscribed to nothing.

### Why the QUEUE publishes the edges

Server-side the edges are published by the approval QUEUE itself
(`PendingEdge::Raised/Answered/Abandoned`), because the five-minute gate timeout
and a `/stop` both retire a prompt through `QueueCleanup::drop` and broadcast no
resolution at all — a mark hung off `ApprovalResolved` would stick forever on
exactly the turns nobody answered.

### Never restored from disk

`approvalPending` is **never decoded from disk**: a parked gate lives in gateway
memory, so a mark restored on an offline cold start could only describe a prompt
that no longer exists.

`-baybo-demo-approval` with `-baybo-open-home` flips three rows live 2s in
(screenshot before/after).

## App-icon badge

`BadgeCenter` (`app/ios/App/Core/BadgeCenter.swift`) is the one writer on the app
side, driven from `SessionIndex.save()` (the funnel every list mutation already
passes through) and counting exactly what the main list counts — archived rows
excluded, coalesced on equality.

The gateway is the other writer, sealing a `badge` into the encrypted preview so
the NSE can set `content.badge` while the app is dead; it counts through the same
`fold_unread` as the chat list's per-row badges, so the icon and the rows cannot
drift into two implementations.

Only `SessionIndex.shared` owns the icon (`ownsAppBadge`) — the suites run in
parallel against temp directories and would otherwise race over the host app's
real badge.

### Full authorization, not provisional

**`AppDelegate.registerForPush` asks for FULL authorization**
(`[.alert, .sound, .badge]`), not provisional: provisional is granted silently
but delivers *quietly* — no sound, no lock-screen alert, and no badge — which is
fine for "your agent replied" and useless for "a tool call denies itself in five
minutes unless you answer".

iOS honours `options` only on the FIRST determination, so an install that already
took provisional on an earlier build does not widen its grant;
`logAuthorizationState` records what was actually granted so that case is
observable instead of presenting as a feature that silently never works.

## Push tap routing

The gateway embeds `session_id` INSIDE the encrypted preview plaintext (never the
outer APNs payload — C stays blind, matching the hashed collapse-id invariant).

The NSE decodes it (optional field; the pinned AEAD fixture predates it and must
keep decoding) and stashes it in the delivered `userInfo` under
`PushPayloadKeys.sessionId` (one file compiled into both targets).

The app's `UNUserNotificationCenterDelegate` routes the tap to that session via
`AppStore.routeToSession` (stash-and-consume across the launch restore);
foreground pushes present nothing.
