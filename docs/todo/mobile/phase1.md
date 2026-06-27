# Mobile companion — Phase 1 (iOS)

Phase 1 ships an iOS companion app for Baybo with two user-facing features:

1. **Scan-to-connect (扫码连接)** — pair the phone to an Baybo gateway by scanning a
   QR code; a SPAKE2 handshake establishes a device identity the operator then
   approves.
2. **Remote notifications (远程通知)** — when an agent turn completes, the phone
   gets a push whose lock-screen preview is **end-to-end encrypted**: the
   gateway encrypts, the operator's relay forwards ciphertext blind, and the
   phone decrypts locally in a Notification Service Extension (NSE).

The app is a **Tauri 2** shell (`app/mobile`) over a host-tested,
FFI-free Rust core (`baybo-mobile-core`); the protocol + crypto live in shared
crates so the phone and the gateway agree by construction.

## Roles

| Role | Who | Trust |
|---|---|---|
| **A** | the Baybo gateway (the user's `baybo` instance) | holds session data; encrypts previews |
| **C** | the operator-run remote host (`remote-host/`) | **blind** relay + APNs sender; sees only ciphertext, never plaintext or keys |
| **P** | the phone (this app) | decrypts previews locally; holds its push key + Noise identity |

C is a **separate Cargo workspace** that deliberately depends on no `baybo-*`
crate — its `/notify` + `/register` payloads are a JSON contract, so the
`.p8`-holding push role stays isolatable.

**Binding is 1:1.** One gateway (A) binds one device (P), and one app binds one
gateway — the gateway is single-user, so the chain is *gateway ↔ user ↔ app*.
A↔P is the only durable binding; C is a shared, blind relay and is **not**
bound 1:1 (one C fronts many gateways, admitted by `instance_key`). Re-pairing
**replaces** the prior binding (newest wins) behind an explicit confirm on both
ends; there is an explicit unpair (*Forget*). The mechanism — the
`idx_devices_one_approved` partial index (one approved row, keyed on `status`),
the atomic `create_replacing_approved` swap at finalize, and the app's single
keychain record + Replace/Forget UX — is specified in
[`remaining.md` §7](remaining.md#7-one-gateway--one-app-11-binding--done).

## Crates

- `crates/wire` — the `Frame` / `Message` WS wire types (shared with the web channel).
- `crates/device-proto` — the device protocol: `pake` (SPAKE2), `noise` (Noise IK
  over `snow`), `aead` (ChaCha20-Poly1305 preview framing), `kdf` (HKDF-SHA256
  → `channel_key` + `push_key`), `pairing` (`PairFrame` / `DeviceHello` /
  `GatewayWelcome` / `ApnsEnv`), and `fixtures` (the pinned cross-language AEAD
  vector).
- `crates/gateway` — A-side: the `/v1/device/pair` route (`channel/device_pair.rs`),
  the device store + mutual-confirm orchestration, and the push dispatcher (`push/mod.rs`).
- `crates/pairing` — the device pairing service.
- `remote-host/crates/push` — C-side APNs: HTTP/2 sender (`apns_http.rs`), ES256
  provider tokens from the `.p8` (`jwt.rs`), the `/notify` + `/register` routes
  (`serve.rs`), token store.
- `remote-host/crates/relay` — C-side blind byte-pipe + SPAKE2 rendezvous (the
  relay-fallback data leg).
- `app/mobile/core` (`baybo-mobile-core`) — P-side: `PairingClient`,
  `ContentSession` (Noise self-pull), and the direct-first/relay-fallback
  `connect` policy. No FFI, no platform APIs — fully host-unit-testable.
- `app/mobile/src-tauri` — the Tauri shell: the `pair` command, push-key
  keychain persistence (`keychain.rs`), and push registration (`push_register.rs`).
- `app/mobile/apple/NotificationExtension` — the NSE (`NotificationService.swift`,
  `PushKeyStore.swift`), plus `verify-crypto.swift` and `verify-nse.sh`.

## Wire & crypto contracts

**Pairing** (`/v1/device/pair`, msgpack `PairFrame` over WS, direct or via C's
relay rendezvous):

```
P → A   Hello { code, pake }                 # SPAKE2 start, keyed by the QR code
A → P   PakeReply { pake }                   # both ends now derive K + the confirmation code
P → A   Sealed( DeviceHello )                # device_id, label, Noise static pubkey, apns_token, env
P → A   Sealed( DeviceConfirm )              # phone user's decision after comparing the confirmation code
A → P   Sealed( GatewayWelcome )             # sent only once the operator confirms too: user_id, active auth_token, gateway static pubkey, relay node, direct candidates
```

Both sides derive the same keys from the SPAKE2 secret via HKDF: a `channel_key`
(seals the K-channel messages), a 32-byte `push_key` (the per-device preview
key), and a short human-comparable **confirmation code** (Bluetooth-style numeric
comparison). Pairing is a live, two-sided confirm: `baybo device pair` stays open,
shows the code, and waits for the operator's `y`; the app shows the same code and
waits for the user's tap. The gateway seals `GatewayWelcome` (with an **active**
`auth_token`) only after both confirm — there is no separate `device approve`
step. The operator's decision crosses to the gateway via the shared pairing slot.

**Push preview** (A encrypts → C relays blind → P's NSE decrypts):

```jsonc
// APNs payload C sends (enc/n copied verbatim from A's /notify body):
{ "aps": { "alert": { "title": "Baybo", "body": "New message" }, "mutable-content": 1 },
  "enc": "<base64 ciphertext||16-byte tag>",   // ChaCha20-Poly1305, empty AAD
  "n":   "<base64 12-byte nonce>",
  "kid": 0,                                     // key epoch (rotation-ready; always 0 in phase 1)
  "bid": "<device_id>" }                        // binding id == device_id
```

`mutable-content: 1` wakes the NSE. It reads the 32-byte `push_key` for `bid`
from the **App Group keychain** (account `baybo.push-key.<bid>`, access group
`group.com.baybo.app`), ChaCha20-Poly1305-opens `enc` with nonce `n`, and rewrites
the visible `title`/`body` from the decrypted `{"title","body"}` JSON. On **any**
failure it keeps the generic placeholder — a bad key / wrong nonce / tamper are
indistinguishable, and the only safe response is "New message".

The Rust producer (`device_proto::aead`) and the Swift consumer (CryptoKit
`ChaChaPoly`) are pinned to one byte-exact vector in `device_proto::fixtures`
(`KEY`/`NONCE`/`PLAINTEXT`/`CIPHERTEXT_HEX`); a drift on either side fails a test.

**Registration** (`/register`): after a successful handshake the gateway
(`HttpApnsRegistrar`, best-effort, gateway-mediated) POSTs `{instance_key,
device_id, apns_token, env}` to C, so the phone never holds a C credential.

## Milestones & status

| # | Scope | Status |
|---|---|---|
| **M0** | Protocol + crypto (`wire`, `device-proto`): SPAKE2, Noise, AEAD, KDF, pinned fixture | ✅ done, host-tested |
| **M1** | Gateway pairing (A): `/v1/device/pair`, device store, mutual confirm | ✅ done |
| **M2** | Remote host (C): push (HTTP/2 APNs, ES256 `.p8` JWT, `/notify`, `/register`) + blind relay | ✅ done |
| **M3** | Tauri iOS app + React UI + NSE Xcode target | ✅ done — `Baybo.app` builds, NSE `.appex` embedded, installs + launches on the iOS 26 simulator |
| **M3.5** | Push-key keychain persistence + provisional notification auth (the NSE keystone) | ✅ implemented + verified to the signing boundary (see below) |
| **M4** | Real end-to-end APNs delivery (device token → C → APNs → device) | ✅ verified on a real provisioned build + device |

## iOS app structure

- `pair` command → `baybo-mobile-core::PairingClient` over a `tokio-tungstenite`
  WS; on success the shell persists `push_key` to the App Group keychain
  (`keychain.rs`, Security-framework `SecItemAdd` from Rust — no Swift added to
  the app target) and returns an operator-pending `PairedSummary` to the webview.
- `push_register.rs` (objc2, iOS-only, from the Tauri `setup` hook): requests
  **provisional** notification authorization (granted silently, no prompt, still
  wakes the NSE) and calls `registerForRemoteNotifications`.
- The NSE is an embedded `app-extension` target (`project.yml`), in the app's
  `PlugIns`, sharing the `group.com.baybo.app` App Group + keychain.

## What's verified, and the external boundary

Verified locally (iOS 26 simulator, Xcode 26):

- Full `Baybo.app` archive builds and embeds `NotificationExtension.appex`.
- The Rust core + keychain + objc2 registration compile, link, and run for
  `aarch64-apple-ios-sim`; clippy-clean across all three workspaces.
- Provisional auth works: after launch, a `simctl push` is **delivered** through
  the usernotifications pipeline to `com.baybo.app` (it is dropped without auth).
- The AEAD interop is byte-exact (`device_proto::fixtures` +
  `verify-crypto.swift`, runnable on macOS without Xcode).

The **App Group keychain** (and thus the live NSE decrypt, and real APNs) is
gated on a **provisioned, code-signed** build — an unavoidable Apple boundary,
nailed down empirically on the iOS 26 simulator:

- Unsigned build → `SecItemAdd` returns **`errSecMissingEntitlement` (-34018)**:
  the code reaches the keychain correctly; only the entitlement is unhonored.
- **`get-task-allow` is mandatory** to launch *any* re-signed build — the
  `--no-sign` linker signature carries it; a `codesign --entitlements` that drops
  it gets the launch denied by `SBMainWorkspace`.
- The simulator **rejects the App Group access group `group.com.baybo.app`**
  (whether in `application-groups` *or* `keychain-access-groups`) **unless the
  App Group is provisioned for the signing team**. Manual `codesign` — ad-hoc
  *or* with a real Development identity — **cannot register an App Group**; the
  launch is denied. (A bare `get-task-allow`-only sign launches fine, isolating
  the App Group as the blocker.)
- `codesign` with a real identity also needs interactive keychain authorization
  (it fails headlessly with `errSecInternalComponent`).

The reliable path is therefore **Xcode automatic signing**: open
`src-tauri/gen/apple/baybo-mobile-app.xcodeproj`, set your team under Signing &
Capabilities for **both** the app and the NSE target — Xcode registers
`group.com.baybo.app` and provisions it (a **paid Apple Developer** capability;
`com.baybo.app` may need a team-unique bundle id, kept in sync with the App Group
id in `PushKeyStore.swift` / `keychain.rs`). `apple/verify-nse.sh` automates the
build + signing + seed + push and, when the App Group is not yet provisioned,
detects the launch denial and prints this path. PASS = a notification reading
*"Baybo / The agent finished replying."*

## M4 work — done (verified on a real provisioned build + device)

All three pieces are implemented and have now been exercised end-to-end by the
operator on a paid Apple Developer account + a real device:

1. **Device-token capture** ✅ — the runtime hooks the Tauri/wry-owned
   `UIApplicationDelegate` via `class_addMethod`, capturing the token iOS delivers
   to `didRegisterForRemoteNotificationsWithDeviceToken` and threading it (hex) +
   the build's APNs env into `DeviceHello` (`push_register.rs`, `pairing.rs`). A
   real token requires a signed build with `aps-environment` (dev account).
2. **`.p8` provisioning** ✅ — the APNs auth key is registered with the operator's
   push role (`remote-host/crates/push`) and the App ID's Push Notifications
   capability.
3. **Device enrollment** ✅ — the live `.p8` send was exercised to a physical
   device; A→C `/register` and the completed-turn → `/notify` → APNs → NSE
   decrypt path are confirmed end-to-end.
