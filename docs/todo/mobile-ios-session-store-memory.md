# iOS Chat-Store Memory: Evict Idle Stores + Cap Offscreen Buffers

## Problem

The per-session `ChatStore` cache and its offscreen frame buffer both grow
without bound over a long-lived app run. The FFI `FrameSink` itself is *not* the
cost — each `ChatStore.Sink` is a weak-ref + `Int` (~tens of bytes), and the
Rust `SessionRegistry.sinks` entry is a `String` + `Arc` — hundreds of them are
tens of KB. The real accumulators are two:

1. **`AppStore.chatStores: [String: ChatStore]` never evicts.** It is only
   cleared on logout/rebind (`resetChatStores`, `AppStore.swift:309`). Every
   session opened in a run leaves a **strong-held** `ChatStore` alive forever.
   Open hundreds of conversations and hundreds of stores stay resident.

2. **`ChatStore.bufferedFrames: [String]` grows unbounded while offscreen.**
   Backing out of a chat only detaches the `TranscriptBridge`; the sink stays
   registered, so frames for a still-subscribed offscreen session buffer in the
   store (`pushFrame`, `ChatStore.swift:187`) until the next `attachBridge`
   flush (`:213`). A long agent turn on a backgrounded session accumulates every
   frame's JSON string in memory; several such sessions compound it.

Both are latent today because iOS only subscribes to sessions the user actually
opens. They become real if (a) a user opens many conversations in one run, or
(b) any future feature widens the subscribed set. (Note: the SessionActivity
list-unread work in
[`mobile-ios-unsubscribed-session-activity.md`](mobile-ios-unsubscribed-session-activity.md)
deliberately does **not** widen it — it is a connection-global broadcast with a
single list sink, so it adds no per-session store or buffer.)

## Proposed Direction

- **LRU-evict `chatStores`.** Keep a bounded working set (e.g. the N most
  recently activated, N ~ 8–16). When a store falls out of the window and is
  offscreen (bridge detached, not the `chatPath` top), `await store.disconnect()`
  — which drops its gateway subscription and Rust sink and cancels its timers —
  then remove it from the map. Re-opening the session mints a fresh store and
  re-subscribes; the transcript mirror + gateway history replay make that a
  cheap catch-up, not a data loss. Eviction must never touch the currently
  pushed session or one with an attached bridge.

- **Cap `bufferedFrames`.** Give the buffer a max length (frames or bytes). On
  overflow, drop the buffer and set a "needs full refetch" flag so the next
  `attachBridge` fetches history via REST/`chatCatchUp` instead of flushing a
  truncated (hole-punched) frame stream. Streamed deltas aren't durable rows, so
  a clean refetch is the only correct recovery — a partial flush would corrupt
  the transcript.

## Open Questions

- Working-set size and whether it should be memory-pressure-driven
  (`didReceiveMemoryWarning`) rather than a fixed N.
- Should eviction proactively `disconnect` (freeing gateway-side subscription +
  in-flight replay buffer too), or just drop the Swift store and let the sink
  linger until the leg cycles? Proactive disconnect is cleaner but costs a
  re-subscribe on return.
- Buffer cap unit: frame count is simplest, byte budget is more honest for
  image-heavy frames.

## Related

- `app/ios/App/Core/AppStore.swift` — `chatStores` map (`:55`), `chatStore(for:)`
  (`:244`), `resetChatStores` (`:309`), `didBecomeActive` reconnect-all (`:139`)
- `app/ios/App/Core/ChatStore.swift` — `bufferedFrames` (`:56`), `pushFrame`
  buffering (`:187`), `flushBufferedFrames` (`:213`), `disconnect` (`:160`),
  `Sink` (`:316`)
- `app/ios/ffi/src/transport.rs` — `SessionRegistry.sinks` (`:177`),
  `disconnect` (`:364`)
- `crates/channels/src/connection.rs` — gateway-side `Connection.subscribed`
  `DashSet` (`:89`)
- `app/ios/CLAUDE.md` — "BayboClient" / offscreen-buffering architecture note
