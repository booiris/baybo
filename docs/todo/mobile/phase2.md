# mobile phase-2 — two-way client (send + polished UI)

> **Status: planning (roadmap altitude).** Builds directly on
> [phase1.md](phase1.md); architecture reference
> [`mobile-remote-host.md`](../../mobile-remote-host.md). Phase 1 proved the pipe
> (pair → blind push → self-pull). Phase 2 makes the app **worth using daily**:
> it sends, renders like the web chat, and acts on notifications. Detail firms up
> when this phase is scheduled.

## Goal

Turn the deliberately-minimal, **receive-only** phase-1 app into a real two-way
client — without re-opening any phase-1 decision (Tauri shell, `aura-mobile-core`
in `src-tauri`, blind C, multi-gateway, the Noise E2E channel).

## Scope

**In:**

1. **Message send.** The composer drives a turn: P emits inbound `Frame::Message`
   / `Frame::Messages` (coalesced send) over the existing E2E Noise channel — the
   same frames the web chat uses (`crates/channels/src/wire.rs`). No new gateway
   surface beyond what `AuthedClient::Device` already scopes (`/v1/chat/*` +
   `/v1/channel-ws`).
2. **Web-chat-parity UI.** Bring the [`web-chat`](../../web-chat.md) experience to
   the Tauri webview: conversation + folder list, pin, streaming assistant render
   (the incremental answer-chunk frames), attachments, model switch,
   slash-command completion, input-history ring, and the **mid-turn interjection
   queue**. Reuse `@aura/channel-sdk` types; the data flow stays
   webview → Tauri command → `aura-mobile-core` → Noise/Frame (not the web's
   direct REST/WS).
3. **Notification mark-read action.** A lock-screen **mark-read**
   `UNNotificationAction` that clears the collapse group — no send, so no Noise
   handshake needed. **Lock-screen text reply is out of scope** (resolved): typing a
   reply from a notification would mean posting to A from a brief background window —
   the same constraint that stops the NSE running Noise — so **sending stays in-app
   via the composer** (item 1).
4. **Push-key rotation.** Activate the `kid` epoch hook reserved in phase 1, as a
   **P-confirmed cutover** (not independent timers, which would drop pushes): P
   derives `kid=N+1`, the **app process** — which owns the shared-Keychain writes;
   the NSE never writes keys and cannot initiate rotation from a push arrival —
   commits it, P then **ACKs the new `kid` to A over Noise**, and only *then* may A
   emit `kid=N+1`. A keeps encrypting under `N` until the ACK; the NSE retains the
   previous `kid` for pushes in flight at the switch. This closes **both** the
   old-key and new-key windows (a "both sides flip on a schedule" design is unsafe —
   there is no atomic cross-process, cross-device commit). Removes phase 1's "static
   key forever" caveat.
5. **Background-job / `SubagentNotification` push.** Extend the phase-1 dispatcher
   filter to also buzz on terminal `shape == Turn && kind == SubagentNotification`
   turns (the `BackgroundJobFinished → SubagentNotification` reply — verified a real
   `Turn`-shaped edge via `agent_loop.rs::run()`). **This consciously re-opens the
   phase-1 filter exclusion** (phase 1 deliberately dropped this trigger). The
   preview must be the **notification's own content**, not "the session's last
   assistant message" (stale/empty for a background buzz), and `collapse_id` must
   keep it from coalescing with a real UserChat turn's notification.

**Out (later phases):** production APNs / App Store (P3); NAT hole-punch (P4);
Android (P5).

## Key decisions / approach (to confirm when scheduled)

- **Send transport = the existing `Frame` inbound path** over Noise. No bespoke
  send API; the device is just another `Subscribed` channel client that can write.
- **UI = port, not fork, of web-chat.** Maximize component/logic reuse from
  `web/src/pages/chat/`; the only swap is the transport adapter (Tauri command +
  `aura-mobile-core` instead of the web REST/WS client). Confirm how much of the
  web React tree can be shared vs. re-implemented for mobile ergonomics.
- **Rotation cadence + ACK ordering.** Pick the interval and lock the
  P-commits-Keychain → P-ACKs-A → A-switches ordering (item 4); decide how many
  previous `kid`s the NSE retains.

## Dependencies

- Phase 1 fully landed (pairing, Noise E2E, self-pull, dispatcher, NSE, multi-gateway).
- `aura-mobile-core` gains: send (`Frame::Message`/`Messages` emit), streaming-chunk
  decode/render feed, attachment upload over the channel.
- Gateway A: dispatcher filter extended to `SubagentNotification`; push-key rotation
  on the A side (new `kid` in `SecretVault`).
- No new C surface (push payload already carries `kid`).

## Landing slices

1. **Send + streaming render** (the core two-way loop) — the app can hold a real
   conversation; testable in Simulator against a local gateway.
2. **UI parity** (folders/pin/attachments/model-switch/slash/history/interjection).
3. **Notification mark-read action** (no lock-screen reply — sending is in-app).
4. **Push-key rotation** + **background-job push** (hardening + the second trigger).

## Open questions

- How much of `web/`'s chat React is portable vs. mobile-rewritten?
- Rotation cadence + ACK ordering + previous-`kid` retention depth.
- SubagentNotification preview content + `collapse_id` namespacing (item 5).

## Related

- [phase1.md](phase1.md) — the pipe this builds on.
- [`web-chat.md`](../../web-chat.md) — the feature set + data-flow being mirrored.
- [`mobile-remote-host.md`](../../mobile-remote-host.md) — architecture; `Frame`,
  `kid`, dispatcher.
