# Mobile Catch-up `Frame::Reset` Recovery

## Problem

When the gateway sends a `Frame::Reset`, the mobile app shows a status string and
does nothing else. The transcript is left stale, and — worse — the catch-up cursor
is left at the offending pre-gap value, so the next reconnect re-triggers the same
Reset: a **loop**. The REST recovery primitives exist on the direct path but are
never called; the relay path has no recovery primitive at all.

`Frame::Reset` fires in two transport-agnostic places:

- **Catch-up overflow** — a `Subscribe` whose `since_ordinal` implies a gap larger
  than the replay cap (`MAX_CATCHUP_REPLAY = 200`,
  `crates/gateway/src/channel/route.rs`). Reason: *"catch-up gap exceeds 200 rows;
  refetch via REST"*.
- **Back-pressure** — an outbound queue overflow (`send_reset` in
  `crates/channels/src/channel.rs`). Reason: *"outbound queue full"*.

The frame carries only `reason` (`crates/wire/src/lib.rs`), and its doc says
clients should *"re-subscribe and refetch session history via the REST
`/v1/chat/sessions/:id` endpoint"* — which, as below, the relay transport cannot do.

## Current behavior (grounded)

- The transport pump forwards `Reset` to the webview unchanged — only `Ping` is
  answered locally (`app/mobile/src-tauri/src/transport.rs`).
- `App.tsx`'s frame switch `case "reset"` only does
  `setStatus(t("chat.streamReset", { reason }))`. `messages` / `lastOrdinal` /
  `sentIds` are untouched, and `lastOrdinal.current` — the value seeded into the
  next `Subscribe` by `connect()` — stays the stale pre-gap ordinal, so a reconnect
  overflows again. **This is the loop.**
- The **direct** REST refetch primitives exist but are **dead code**:
  `directHistory()` (`app/mobile/src/direct.ts`) has **zero callers**; it routes to
  the `direct_history` Tauri command (`app/mobile/src-tauri/src/lib.rs`) →
  `direct::history()` (`app/mobile/src-tauri/src/direct/mod.rs`), an admin-Bearer
  `GET /v1/chat/sessions/{id}`.
- The **relay** transport has **no** REST history primitive: its only credential is
  the Noise device leg (no admin token), and there is **no content-leg
  transcript-fetch frame**. The only history frames in `wire`,
  `Frame::HistoryAppend` / `Frame::HistorySnapshot`, are the **TUI input-line ring**
  (`entries: Vec<String>` — submitted command lines, not messages), not transcript
  history.

So the two transports need *different* recovery, and that asymmetry is the design
crux.

## The REST surface (direct path)

`GET /v1/chat/sessions/{id}` — admin Bearer (`crates/gateway/src/auth/admin.rs`).
Params `before_ordinal` + `limit`; **default page 50, max 200**
(`crates/gateway/src/api/admin/chat.rs`). Returns `ChatSessionDetail`:

- `transcript: ChatTranscriptItem[]` — `kind` `message` / `work` / `notice`,
  `role`, `text`, `ordinal`, `has_attachments` (boolean — **no blob refs**),
  oldest-first within the page.
- `oldest_ordinal` / `newest_ordinal` — real `session_messages.ordinal` (`null`
  for an empty page); seed pagination from `oldest`, seed the WS cursor from
  `newest`.
- `has_more` — at least one older row exists below this page.
- Control-event items carry **synthetic negative ordinals** — never use them for a
  cursor.

Key consequence: Reset fires at gap > 200, but the default page is 50, so **one
default fetch recovers only the newest 50** (`has_more = true`). Mobile has no
scroll-up pagination today.

## `app/web` reference recovery (the model to mirror)

The web client (`app/web/src/api/chatWs.ts`, `app/web/src/pages/ChatPage.tsx`) on
`reset`: clears the per-session WS cursors (so it can't reuse the offending one),
wipes the `SessionView` to `EMPTY_VIEW` (preserving pending approvals), cancels
stream pacers, and bumps a `historyEpoch`. The history-load effect then refetches
`GET /v1/chat/sessions/{id}` (no params), maps the transcript via
`historyRowToTranscript` (work → collapsed block, notice → card, else bubble), and
**reseeds the WS cursor to `newest_ordinal`** so the post-reset `Subscribe` carries
`since_ordinal = newest` and replays only above it. `has_attachments` rows render an
`[attachment]` placeholder (REST omits blob refs).

## Proposed direction

### Direct transport (v1 — small, self-contained)

Wire `App.tsx`'s `reset` case to a `recoverFromReset(sessionId)` helper, mirroring
web but adapted to mobile's `useRef` cursor model:

1. **Re-entrancy guard** — a `recovering` ref so overlapping Resets don't stack
   concurrent fetches.
2. `const detail = await directHistory(sessionId)` (newest page; `direct.ts`).
3. **Rebuild `messages`** from `detail.transcript` (ascending): `message` → bubble,
   `notice` → notice bubble (matching the live `case "notice"` path), `work` →
   **skip** (mobile has no work concept — the live switch already drops
   reasoning/tool). `setMessages(rebuilt)`; `setStreaming("")`.
4. **Reset cursor + dedup** — `lastOrdinal.current = detail.newest_ordinal ?? 0`
   (real ordinal; ignore the synthetic negatives). Rebuild `sentIds` from the
   user-role rows so the post-reconnect echo of our own messages still de-dups.
   Persist via `saveChatState`.
5. **Force a reconnect** — bump `connEpoch` so `connect()` re-dials with
   `sinceOrdinal = newest_ordinal`; the gateway now replays only rows above the
   refetched tail → no overflow → **loop broken**.
6. `setStatus(null)` on success.

**Gotchas:**

- **Attachments are lost on rebuild** — REST history carries only
  `has_attachments`, no blob refs. Drop them or render an `[attachment]`
  placeholder (web does the latter; recommend the placeholder for honesty).
- **`has_more` after a > 200 gap** — a default 50-row fetch recovers only the newest
  50, and mobile has no scroll-up paging yet. v1 shows the newest 50; older history
  is a deferred paging UI.
- **Repeated Resets** (queue-full back-pressure) — debounce/back off rather than
  tight-looping; step 4's cursor reset is what actually breaks the catch-up loop.

**Scope:** ~40–70 lines, `App.tsx` only, no Rust changes (`direct_history` /
`directHistory` already exist). Branch on the active transport (`CHAT_MODE_KEY`) so
only direct sessions call `directHistory`.

### Relay transport (the unsolved half — needs a real decision)

Relay has no admin REST and no content-leg transcript-fetch frame, so
`directHistory` cannot be reused. Three honest options:

- **(R1) Degrade to live-only, no wire change — recommended v1.** On a relay Reset,
  drop the stale cursor and reconnect with `since_ordinal = None` (a fresh
  subscribe — `None` never triggers Reset). Show a status like *"earlier messages
  unavailable — showing live messages."* Result: no loop, live chat resumes; the
  > 200-row gap is a visible hole, not backfilled. Requires threading an
  `Option<i64>` through `chat_connect` (today the TS always sends a number).
  Smallest change; honest degradation.
- **(R2) Add a content-leg history-fetch frame — true parity, larger.** New wire
  frames (e.g. client→server `FetchHistory { session_id, before_ordinal, limit }`,
  server→client a history page — possibly reusing `Frame::Messages`), served from
  the same store path the REST endpoint uses but sealed over Noise. This preserves
  E2E and gives relay the same paged backfill as direct. The right long-term design,
  but it touches `wire` + the gateway channel router + `baybo-mobile-core` + both
  transports.
- **(R3) Extend `Reset` with `newest_ordinal`.** Add the field so any client can
  reseed its cursor to live precisely (cleaner than R1's `None`). Small wire change;
  still no backfill. Combinable with R1.

**Recommendation:** ship **R1** alongside the direct v1 now — together they kill the
loop and restore live chat on both transports with zero gateway work — and file
**R2** as the parity follow-up (the only option that actually backfills the relay
gap).

## Implementation order

1. **Direct v1** — `App.tsx` `recoverFromReset`, direct-only, branch on transport.
2. **Relay R1** — thread `Option<i64>` `sinceOrdinal`; relay Reset → reconnect with
   `None` + a "history truncated" status.
3. **Follow-up** — R2 content-leg history frame for relay parity; scroll-up
   pagination (`has_more`) for both transports.

## Open questions

- On a relay Reset, preserve the pre-gap local messages (a visible hole) or wipe to
  live-only? (R1 preserves; web wipes to `EMPTY_VIEW`.)
- Cursor seeding: `since_ordinal` replays **strictly above**, so seeding `=
  newest_ordinal` is correct (no double-render of the newest row) — confirm against
  the relay replay path.
- `work`-item rendering on rebuild: skip (current behavior) vs render `text` as a
  plain assistant bubble.

## Related

- [`../modules/mobile/relay-push-security.md`](../modules/mobile/relay-push-security.md)
  — "Direct-mode push" (the other direct-transport follow-up, now implemented).
- [`../modules/mobile/companion.md`](../modules/mobile/companion.md) — companion
  architecture; app lifecycle, catch-up, and reconnect.
- `crates/gateway/src/channel/route.rs` — `MAX_CATCHUP_REPLAY` and the Reset trigger.
- `crates/gateway/src/api/admin/chat.rs` — `GET /v1/chat/sessions/{id}` and
  `ChatSessionDetail`.
- `app/mobile/src/App.tsx` — the `ChatView` frame switch and `connect()` cursor.
