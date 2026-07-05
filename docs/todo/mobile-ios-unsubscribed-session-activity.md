# iOS Live Unread for Unsubscribed Sessions (SessionActivity → chat list)

## Problem

While the user is in one conversation, a *different* session that was never
opened on this device (so never `Subscribe`d on the live chat leg) can complete
a turn and produce a reply — and iOS gets **no live signal** for it. Two gates
close today:

1. **Gateway fan-out is subscription-gated.** For `Subscribed`-kind channels,
   `Channel::dispatch_event` (`crates/channels/src/channel.rs:371`) delivers a
   session's agent frames only to connections in `subscriptions[session_id]`.
   iOS never subscribed → its connection isn't in the set → the frames are
   dropped at the gateway.

2. **The one cross-session broadcast isn't emitted on the device channel, and
   iOS would drop it anyway.** `Frame::SessionActivity` (a throttled, contentless
   "session X had activity" ping; `session_pulse.rs`) is broadcast to every
   connection regardless of subscription — but `SessionPulse` is installed
   **only on the `http` (web) channel** (`boot.rs:98`, the `is_http` gate). Both
   iOS legs attach to `ChannelType::device()` (`relay_e2e.rs:255`,
   `device_content.rs:105`), which never emits it. Even if it did, iOS's
   `dispatch_inbound_frame` (`app/ios/ffi/src/transport.rs:588`) routes by
   `routing_session_id()` → per-session sink and drops a frame with no sink.

Result: iOS learns of other sessions' completions only via APNs push + the
pull-based chat-list REST refresh (`ChatListScreen.refresh`). The web chat has
live unread badges from `SessionActivity`; iOS has none. Goal: parity — a live
unread badge on background rows.

## Proposed Direction

Layered, gateway → transport → UI. The two prerequisites are the hard part; the
UI is small.

- **Prereq A — gateway emits SessionActivity on `device`.** Broaden the
  `is_http` gate in `install_channel` (`boot.rs:98`) to any `Subscribed` channel
  (or explicit `http | device`), installing a `SessionPulse` per channel (each
  with its own throttle window). `Frame::SessionActivity` is already in the
  shared `wire` crate, so iOS needs no wire change.

- **Prereq B — the leg must be live to receive the broadcast.** Relay warms the
  content leg via `relay_preconnect()` on launch/foreground, so it receives
  broadcasts even with no chat open. Direct has **no** preconnect — while parked
  on the list with no chat open there is no leg. Add a `direct_preconnect()`
  (the generic `SessionRegistry::preconnect`, `transport.rs:194`, already
  exists) and call it from `AppStore.didBecomeActive`/`restoreOnLaunch` for
  direct bindings. Without it, direct users get live unread only when a chat leg
  is already up.

- **Transport — route SessionActivity to a connection-global list sink.** Add a
  UniFFI callback trait alongside `FrameSink`:
  `SessionListSink { fn on_activity(session_id, source, at_millis) }`, registered
  once on the shared `BayboClient` via a new `set_session_list_sink(...)`. In
  the shared `pump`/`dispatch_inbound_frame`, special-case
  `Frame::SessionActivity` → list sink; all other frames keep the per-session
  path. One sink suffices (only one leg is active at a time). Optionally route
  `SessionPatch`/`session_updated` the same way later (list add/remove), out of
  scope here.

- **SessionIndex — hold local unread.** Add `var unread: Int` to `SessionRow`
  (persisted in `sessions.json`). `noteActivity(sessionId:source:at:)` bumps
  `lastActive` (max) and, when the session isn't the current foreground one,
  `unread += 1`. Ignore activity for unknown ids (draft/cron/other-device —
  the REST merge surfaces them later), mirroring web's `applySessionActivity`
  (`ChatPage.tsx:3775`). Critically, `merge(remote:)` must carry local `unread`
  across (it currently rebuilds rows and would drop it) — unread is local-only,
  the server never sends it.

- **Clear on open.** In `AppStore.activateSession` (`:298`) / `ChatScreen`
  appear, `clearUnread(sessionId:)` and mark it the foreground session; clear
  the foreground marker on exit.

- **UI badge.** In `SessionRowView` (`ChatListScreen.swift:110`) add a trailing
  monochrome badge (ink-filled dot / count pill, `99+` cap) shown when
  `unread > 0 && row != active`. Sort already keys on `lastActive`, which the
  ping bumps, so touched rows float up automatically.

Change footprint: gateway 1 site, ffi ~3 (trait, setter, dispatch case; +direct
preconnect), Swift ~4 (sink class, `SessionRow.unread`, SessionIndex logic +
`merge`, row UI + clear-on-open).

## Open Questions

- Ship relay-only first (Prereq A + transport + UI) and add direct preconnect
  (Prereq B) as a follow-up? Relay already satisfies the liveness precondition.
- Count `user`-source pings? Web bumps on both user and assistant when not
  foregrounded; on iOS a `user` ping is this account's echo from another device
  — arguably still worth surfacing, but assistant-only is defensible.
- Persist unread across cold start (in `sessions.json`) vs. reset on launch.
  Persisting matches "you have N unread since you last looked."
- Badge affordance in the monochrome system: dot vs. count. Count is more
  informative; a dot is quieter and more on-brand.

## Related

- `crates/gateway/src/channel/boot.rs` — `install_channel` `is_http` gate (`:98`)
- `crates/gateway/src/channel/session_pulse.rs` — the pulse throttle/emit
- `crates/channels/src/channel.rs` — `dispatch_event` subscription gate (`:371`),
  `broadcast_session_activity`, `SubscribedView`
- `crates/wire/src/lib.rs` — `Frame::SessionActivity` (`:777`), `ActivityKind`,
  `routing_session_id` (`:829`)
- `app/ios/ffi/src/transport.rs` — `dispatch_inbound_frame` (`:588`),
  `SessionRegistry::preconnect` (`:194`)
- `app/ios/ffi/src/api.rs` — `FrameSink` trait (add `SessionListSink` nearby)
- `app/ios/App/Core/SessionIndex.swift` — `SessionRow`, `merge`, `recordUserSend`
- `app/ios/App/Screens/ChatListScreen.swift` — `SessionRowView` (`:110`)
- `app/ios/App/Core/AppStore.swift` — `activateSession` (`:298`),
  `didBecomeActive` (`:130`)
- `app/web/src/pages/ChatPage.tsx` — reference impl: handler (`:721`),
  `applySessionActivity` (`:3775`); `app/web/src/pages/chat/types.ts` — local-only
  `unread` field
