# Mobile Catch-up `Frame::Reset` Recovery — remaining follow-ups

## Status

**v1 shipped** (`app/mobile/src/App.tsx`, `recoverFromReset`). A gateway
`Frame::Reset` (catch-up gap over `MAX_CATCHUP_REPLAY = 200`, or outbound
back-pressure) is now handled instead of just shown as a status string — which had
left the stale pre-gap cursor in place so the next reconnect re-overflowed: a
**loop**. What landed:

- **Direct** — refetch the newest transcript page via `directHistory()` (admin
  Bearer `GET /v1/chat/sessions/{id}`), rebuild the thread (`message` → bubble,
  `notice` → notice bubble, `work` → skipped, `has_attachments` → `[attachment]`
  placeholder), reseed the cursor to `newest_ordinal`, and force a reconnect. The
  follow-up `Subscribe` replays strictly above the tail — nothing — so the gap
  can't re-trigger the Reset.
- **Relay (R1)** — no admin REST and no content-leg history frame, so it degrades
  to live-only: keep the pre-gap messages as a visible hole, drop the cursor to
  `null` (a fresh subscribe never overflows), mark the gap with a notice, and
  re-subscribe.

Resolved along the way (were open questions): cursor seeding `= newest_ordinal` is
correct because catch-up replays **strictly above** (`ordinal > ?2` in
`crates/storage/src/libsql/session.rs`); relay **preserves** the pre-gap messages
(doesn't wipe to live-only like web); `work` items are **skipped** on rebuild.

The two items below are the deferred parity/UX work.

## R2 — content-leg history frame for relay parity (the real backfill)

R1 leaves the relay gap as a hole; it never backfills. Relay can't reuse
`directHistory` because its only credential is the Noise device leg (no admin
token), and there is **no content-leg transcript-fetch frame**. The only history
frames in `wire`, `Frame::HistoryAppend` / `Frame::HistorySnapshot`, are the **TUI
input-line ring** (`entries: Vec<String>` — submitted command lines), not
transcript history.

**Design:** add new wire frames — e.g. client→server
`FetchHistory { session_id, before_ordinal, limit }`, server→client a history page
(possibly reusing `Frame::Messages`) — served from the **same store path** the REST
endpoint uses but **sealed over Noise**, preserving E2E. This is the only option
that actually backfills the relay gap, and gives relay the same paged history as
direct. Touches `wire` + the gateway channel router (`crates/gateway/src/channel/`)
+ `baybo-mobile-core` + both mobile transports.

The page shape to mirror is the REST one (`GET /v1/chat/sessions/{id}`,
`crates/gateway/src/api/admin/chat.rs`, `ChatSessionDetail`):

- `transcript: ChatTranscriptItem[]` — `kind` `message` / `work` / `notice`,
  `role`, `text`, `ordinal`, `has_attachments` (no blob refs), oldest-first.
- `oldest_ordinal` / `newest_ordinal` — real `session_messages.ordinal` (`null` for
  an empty page). Page older from `oldest`; seed the WS cursor from `newest`.
- `has_more` — at least one older row exists below this page.
- Control-event items carry **synthetic negative ordinals** — never use them for a
  cursor.

**Optional refinement (R3):** extend `Frame::Reset` with a `newest_ordinal` field
so any client can reseed its cursor to live precisely, rather than R1's `null`.
Small wire change; still no backfill on its own; combinable with the relay path.

## Scroll-up pagination (`has_more`) — both transports

`Frame::Reset` fires at a gap > 200, but the default REST page is 50, so a single
default fetch recovers only the newest 50 (`has_more = true`). Mobile shows the
newest page plus an "earlier messages aren't shown" notice; there is **no scroll-up
paging** yet. `directHistory(sessionId, beforeOrdinal, limit)` already takes the
params — the missing piece is a chat-log UI that loads older pages from
`before_ordinal = oldest_ordinal` when the user scrolls to the top. For relay this
depends on R2's history frame.

## Related

- [`../modules/mobile/companion.md`](../modules/mobile/companion.md) — companion
  architecture; app lifecycle, catch-up, and reconnect.
- `crates/gateway/src/channel/route.rs` — `MAX_CATCHUP_REPLAY` and the Reset trigger.
- `crates/gateway/src/api/admin/chat.rs` — `GET /v1/chat/sessions/{id}` and
  `ChatSessionDetail`.
- `app/mobile/src/App.tsx` — `recoverFromReset` (the v1 baseline) and the
  `ChatView` frame switch / `connect()` cursor.
