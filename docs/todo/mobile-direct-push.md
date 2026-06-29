# Mobile Direct-Mode Push Notifications

> **Status: implemented.** Built per the "Proposed direction" below, with the
> recommended choices: **Decision 0 = (A)** reuse the remote host C (new `[push]`
> gateway config supplies its `relay_url` + `remote_api_key`); **Piece 3 = (3a)**
> the `push_key` is generated on-device and delivered over the admin-token TLS
> REST channel (encrypted previews preserved); and the web identity mints a real
> Ed25519 key + delegation, so C and the NSE are **unchanged** and the guest-tier
> binding-integrity guarantee (Claim 4) still holds.
>
> Shipped surface: gateway `crates/gateway/src/api/admin/push.rs`
> (`GET /v1/push/params`, `POST /v1/push/register`),
> `crates/gateway/src/push/web.rs`, `baybo_config::PushConfig`; app
> `app/mobile/src-tauri/src/direct/push.rs` + the `direct_push_register` command.
> Security/trust model: the **Direct-mode push** section of
> [`../modules/mobile/relay-push-security.md`](../modules/mobile/relay-push-security.md).
> The sections below are kept as the design rationale.

## Problem

Direct-mode (web-identity) chat sessions receive **no** iOS push notifications.
Only the scan-to-pair (Noise device) path delivers push. A user who connects the
companion via "type a Baybo URL + admin token" (the `directConnect` flow in
`app/mobile/src/App.tsx`, backed by `app/mobile/src-tauri/src/direct/`) gets
**foreground-only** chat: when the app is backgrounded, a completed agent turn
never reaches the lock screen.

This is an inherent design gap, not a regression — the push pipeline is welded to
the Noise device identity that direct mode deliberately never creates. Building
direct-mode push is a feature spanning the gateway, the iOS app, and the NSE, with
a real security trade-off versus the relay path. This doc records why the gap
exists and what closing it would take.

## Why direct mode gets no push today

The entire push path keys off the **`device_id`** established at pairing, and the
**`push_key`** derived from the pairing Noise handshake hash `h`. Direct mode has
neither.

- **Push keys are keyed strictly by `device_id`.** The dispatcher loads the
  per-device key from the vault (`device.{device_id}.push_key`) and `dispatch_to_device`
  needs `device.relay_url`, `device.remote_api_key`, that push key, and the
  pairing-time delegation signature (`crates/gateway/src/push/mod.rs`). The
  per-turn fan-out enumerates only `DeviceStatus::Approved` **device rows** — there
  is no per-session or per-channel-identity push path.
- **`device_id` + `push_key` exist only after scan-to-pair.** Both are created
  during pairing — the device row in `DevicePairingService::complete` /
  `crates/gateway/src/channel/device_pair.rs`, the `push_key` HKDF'd from the
  handshake hash `h`. On device, the NSE's key is written to the shared App-Group
  keychain (`baybo.push-key.{device_id}`) only on a successful pair
  (`app/mobile/src-tauri/src/relay/pairing.rs`, `keychain.rs`); *Forget* deletes it.
- **The web identity has none of this.** `web/<uuid>` is resolved from the
  channel-token table and typed `Web { label, token }`, with **no** `device_id`
  (`crates/gateway/src/auth/channel.rs`). Direct login stores only `base_url` +
  admin token in the keychain — no push key, no device row, no pairing
  (`app/mobile/src-tauri/src/direct/mod.rs`).
- **Direct mode never even registers an APNs token.** `Frame::UpdateApnsToken` is
  intercepted only on the Noise device-content leg
  (`crates/gateway/src/channel/device_content.rs`), and the wire doc says so
  explicitly — *"Handled on the device content leg before the generic router;
  other channels never send it"* (`crates/wire/src/lib.rs`). The relay leg's
  `establish()` pushes `apns_token_frame` in `opening_best_effort`
  (`app/mobile/src-tauri/src/relay/chat.rs`); the direct leg leaves
  `opening_best_effort` empty and registers the sender as a `web` client
  (`web_user_message_frame`, `app/mobile/src-tauri/src/direct/chat.rs`).

So three things are simultaneously absent for a web session: a **registered APNs
token**, a **push key** to encrypt with, and a **device binding** to target.
Nothing is half-wired that "should" fire.

The relay path's full design and threat model are in
[`../modules/mobile/relay-push-security.md`](../modules/mobile/relay-push-security.md);
this doc assumes that as the baseline to diverge from.

## What "build direct-mode push" requires

Three coordinated pieces, plus one architecture decision direct mode forces up
front.

### Decision 0 — who delivers to APNs

In the relay design the **gateway never holds the APNs `.p8`**; the operator-run
remote-host **C** does, and the gateway POSTs ciphertext to C's `/notify`
(`remote-host/crates/push/`). Direct mode has no pairing and may have **no
remote-host at all** — the whole point is a NAT-free direct connection. Two paths:

- **(A) Reuse C for push only.** The gateway is still configured with a
  `remote_api_key` + `relay_url` for the push leg even though chat is direct. Push
  reuses the existing `/register` + `/notify` pipeline, keyed by a synthetic
  **web-binding id** instead of a `device_id`. Smallest new surface; keeps the
  `.p8` out of the gateway. Cost: the operator must run/point at a remote-host for
  push even in direct mode.
- **(B) Gateway-direct APNs.** The gateway itself holds a `.p8` and talks to APNs
  directly. No remote-host needed, but it widens the gateway's credential surface
  and duplicates the whole APNs sender (ES256 `.p8` JWT, HTTP/2, token pruning)
  that lives in C today.

**Recommendation: (A).** Far less new code, and it preserves the "gateway never
holds the provider key" property the relay design relies on.

### Piece 1 — register an APNs token for the web identity

The app must get its APNs device token to the gateway over the web identity.
Either accept `Frame::UpdateApnsToken` on the `/v1/channel-ws` web path (today
rejected off the device leg), or add a small **admin-Bearer REST** call (e.g.
`POST /v1/chat/apns`) the app makes at direct-login / chat-connect. REST fits
direct mode's existing admin-token model and avoids touching the web channel-ws
router. The direct leg's `establish()` must then actually send the token (it sends
nothing today).

### Piece 2 — a push target the dispatcher can reach

The dispatcher fans out to approved **device rows**; a web session is not one.
Introduce a **web-push binding**: a record keyed by the web install (not the
per-chat session — channel tokens and session ids are too ephemeral), carrying
`apns_token`, `env`, `push_key`, and the `relay_url` + `remote_api_key` needed to
reach C. The dispatch trigger (completed `UserChat` turn → `reply_ordinal`,
`crates/gateway/src/push/mod.rs`) is already session-scoped; the missing link is
mapping a session to the web binding(s) that should be notified. Unlike the
relay path's strict **1:1** binding, direct mode is many web identities against one
gateway, so the fan-out must enumerate web bindings rather than assume one.

### Piece 3 — a push key the NSE can decrypt with

This is the deepest change and the real security trade-off. The relay `push_key`
is HKDF'd from the Noise handshake hash `h` — a forward-secret, mutually-
authenticated secret neither C nor anyone else can compute. Direct mode has no
handshake, so there is no such secret. Options:

- **(3a) Negotiate a push key over the authenticated TLS REST channel** at direct
  login: the gateway mints a 32-byte key, returns it in the (admin-Bearer, TLS)
  login response, the app stores it in the App-Group keychain under a web-scoped
  account, and the gateway stores it on the web binding. The NSE is already
  **`bid`-keyed** (`baybo.push-key.<bid>` via `PushKeyStore.swift`), so if
  `/notify` sets `bid` to the web-binding id and the app stored the key under
  `baybo.push-key.<that-bid>`, the NSE decrypt path works **unchanged**.
  *Trade-off:* the key's confidentiality now rests on TLS + the admin bearer token,
  not a Noise handshake — strictly weaker, and the gateway (not just the endpoints)
  mints the secret.
- **(3b) Generic-body-only / unencrypted previews.** Skip preview encryption for
  direct mode; send a generic `"New message"` (or, worse, plaintext). Simplest, but
  any real preview body now passes through C and APNs in the clear — a downgrade
  from the E2E guarantee the relay path documents
  ([relay-push-security.md](../modules/mobile/relay-push-security.md), Claim 3).

**Recommendation: (3a)** if previews should stay private to the endpoints, paired
with a clear note that the trust model is TLS-bearer, not Noise. Offer
generic-body-only as the safe default so a privacy-conscious operator can ship
push without any plaintext leaving the gateway.

### Optional — binding integrity under a shared `remote_api_key`

The relay path's guest-tier protection
([relay-push-security.md](../modules/mobile/relay-push-security.md), "Guest-tier
tenancy" / Claim 4) rests on a per-device **Ed25519 delegation chain**, so a
co-tenant under the shared `guest` key cannot hijack a binding. A web identity has
no Ed25519 key, so a naive web binding loses that property: under a shared
`remote_api_key`, a co-tenant who learns the web-binding id could register over /
redirect / suppress it. To keep parity, either mint an Ed25519 key for the web
install and reuse the same delegation chain, or **require a registered-tier
(non-shared) `remote_api_key`** for direct-mode push and document that guest-tier
direct push is unprotected.

## Proposed direction (phased)

1. **Privacy posture** — encrypted previews (3a) vs generic-body-only (3b). A
   product/security call that gates everything else.
2. **Gateway** — web-push binding store; APNs registration entry point
   (admin-Bearer REST); push-key mint at login; dispatch mapping session → web
   binding; reuse C's `/register` + `/notify` pipeline keyed by the web-binding id.
3. **App** — send the APNs token at direct login/connect; store the gateway-minted
   push key in the App-Group keychain under the web-scoped account.
4. **NSE** — unchanged if the `bid` and keychain account align; only touched if we
   pick a different naming scheme.
5. **Security doc** — add a "Direct-mode push" section to
   [relay-push-security.md](../modules/mobile/relay-push-security.md) spelling out
   the weaker (TLS-bearer, no-Noise) trust model and the guest-tier caveat.

## Open questions

- **Lifecycle** — bind per install or per session? (Recommend per install at
  direct-login; sessions are gateway-minted per chat and too ephemeral.)
- **Multiplicity** — relay is strictly 1:1; direct is many web identities per
  gateway. Push fan-out must enumerate web bindings, not assume one.
- **Remote-host availability** — does the operator always have a C for direct mode?
  If not, Decision 0 collapses to option (B) and scope grows.
- **Token rotation** — APNs tokens rotate (reinstall/restore/new device). Direct
  mode needs the same re-register-on-change the device leg gets via
  `Frame::UpdateApnsToken`.

## Related

- [`../modules/mobile/relay-push-security.md`](../modules/mobile/relay-push-security.md)
  — the relay push design, threat model, and the delegation chain this would weaken.
- [`../modules/mobile/companion.md`](../modules/mobile/companion.md) — companion
  architecture and the 1:1 binding direct mode breaks.
- [`mobile-reset-recovery.md`](mobile-reset-recovery.md) — the other open
  direct-transport follow-up.
- `crates/gateway/src/push/mod.rs` — the device-keyed push dispatcher to generalize.
- `app/mobile/src-tauri/src/direct/` — the direct (web-identity) transport.
