# Mobile companion — Phase 1 (iOS)

Phase 1 ships an iOS companion app for Aura with two user-facing features:

1. **Scan-to-connect (扫码连接)** — pair the phone to an Aura gateway by scanning a
   QR code; a SPAKE2 handshake establishes a device identity the operator then
   approves.
2. **Remote notifications (远程通知)** — when an agent turn completes, the phone
   gets a push whose lock-screen preview is **end-to-end encrypted**: the
   gateway encrypts, the operator's relay forwards ciphertext blind, and the
   phone decrypts locally in a Notification Service Extension (NSE).

The app is a **Tauri 2** shell (`app/mobile/ios`) over a host-tested,
FFI-free Rust core (`aura-mobile-core`); the protocol + crypto live in shared
crates so the phone and the gateway agree by construction.

## Roles

| Role | Who | Trust |
|---|---|---|
| **A** | the Aura gateway (the user's `aura` instance) | holds session data; encrypts previews |
| **C** | the operator-run remote host (`remote-host/`) | **blind** relay + APNs sender; sees only ciphertext, never plaintext or keys |
| **P** | the phone (this app) | decrypts previews locally; holds its push key + Noise identity |

C is a **separate Cargo workspace** that deliberately depends on no `aura-*`
crate — its `/notify` + `/register` payloads are a JSON contract, so the
`.p8`-holding push role stays isolatable.

## Crates

- `crates/wire` — the `Frame` / `Message` WS wire types (shared with the web channel).
- `crates/device-proto` — the device protocol: `pake` (SPAKE2), `noise` (Noise IK
  over `snow`), `aead` (ChaCha20-Poly1305 preview framing), `kdf` (HKDF-SHA256
  → `channel_key` + `push_key`), `pairing` (`PairFrame` / `DeviceHello` /
  `GatewayWelcome` / `ApnsEnv`), and `fixtures` (the pinned cross-language AEAD
  vector).
- `crates/gateway` — A-side: the `/v1/device/pair` route (`channel/device_pair.rs`),
  the device store + operator approval, and the push dispatcher (`push/mod.rs`).
- `crates/pairing` — the device pairing service.
- `remote-host/crates/push` — C-side APNs: HTTP/2 sender (`apns_http.rs`), ES256
  provider tokens from the `.p8` (`jwt.rs`), the `/notify` + `/register` routes
  (`serve.rs`), token store.
- `remote-host/crates/relay` — C-side blind byte-pipe + SPAKE2 rendezvous (the
  relay-fallback data leg).
- `app/mobile/ios/core` (`aura-mobile-core`) — P-side: `PairingClient`,
  `ContentSession` (Noise self-pull), and the direct-first/relay-fallback
  `connect` policy. No FFI, no platform APIs — fully host-unit-testable.
- `app/mobile/ios/src-tauri` — the Tauri shell: the `pair` command, push-key
  keychain persistence (`keychain.rs`), and push registration (`push_register.rs`).
- `app/mobile/ios/apple/NotificationExtension` — the NSE (`NotificationService.swift`,
  `PushKeyStore.swift`), plus `verify-crypto.swift` and `verify-nse.sh`.

## Wire & crypto contracts

**Pairing** (`/v1/device/pair`, msgpack `PairFrame` over WS, direct or via C's
relay rendezvous):

```
P → A   Hello { code, pake }                 # SPAKE2 start, keyed by the QR code
A → P   PakeReply { pake }
P → A   Sealed( DeviceHello )                # device_id, label, Noise static pubkey, apns_token, env
A → P   Sealed( GatewayWelcome )             # user_id, auth_token (inert until approved), gateway static pubkey, relay node, direct candidates
```

Both sides derive the same keys from the SPAKE2 secret via HKDF: a `channel_key`
(seals the two `DeviceHello`/`GatewayWelcome` messages) and a 32-byte `push_key`
(the per-device preview key). The `auth_token` is inert until the operator runs
`aura device approve`.

**Push preview** (A encrypts → C relays blind → P's NSE decrypts):

```jsonc
// APNs payload C sends (enc/n copied verbatim from A's /notify body):
{ "aps": { "alert": { "title": "Aura", "body": "New message" }, "mutable-content": 1 },
  "enc": "<base64 ciphertext||16-byte tag>",   // ChaCha20-Poly1305, empty AAD
  "n":   "<base64 12-byte nonce>",
  "kid": 0,                                     // key epoch (rotation-ready; always 0 in phase 1)
  "bid": "<device_id>" }                        // binding id == device_id
```

`mutable-content: 1` wakes the NSE. It reads the 32-byte `push_key` for `bid`
from the **App Group keychain** (account `aura.push-key.<bid>`, access group
`group.com.aura.app`), ChaCha20-Poly1305-opens `enc` with nonce `n`, and rewrites
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
| **M1** | Gateway pairing (A): `/v1/device/pair`, device store, operator approval | ✅ done |
| **M2** | Remote host (C): push (HTTP/2 APNs, ES256 `.p8` JWT, `/notify`, `/register`) + blind relay | ✅ done |
| **M3** | Tauri iOS app + React UI + NSE Xcode target | ✅ done — `Aura.app` builds, NSE `.appex` embedded, installs + launches on the iOS 26 simulator |
| **M3.5** | Push-key keychain persistence + provisional notification auth (the NSE keystone) | ✅ implemented + verified to the signing boundary (see below) |
| **M4** | Real end-to-end APNs delivery (device token → C → APNs → device) | ⚠️ blocked on external resources |

## iOS app structure

- `pair` command → `aura-mobile-core::PairingClient` over a `tokio-tungstenite`
  WS; on success the shell persists `push_key` to the App Group keychain
  (`keychain.rs`, Security-framework `SecItemAdd` from Rust — no Swift added to
  the app target) and returns an operator-pending `PairedSummary` to the webview.
- `push_register.rs` (objc2, iOS-only, from the Tauri `setup` hook): requests
  **provisional** notification authorization (granted silently, no prompt, still
  wakes the NSE) and calls `registerForRemoteNotifications`.
- The NSE is an embedded `app-extension` target (`project.yml`), in the app's
  `PlugIns`, sharing the `group.com.aura.app` App Group + keychain.

## What's verified, and the external boundary

Verified locally (iOS 26 simulator, Xcode 26):

- Full `Aura.app` archive builds and embeds `NotificationExtension.appex`.
- The Rust core + keychain + objc2 registration compile, link, and run for
  `aarch64-apple-ios-sim`; clippy-clean across all three workspaces.
- Provisional auth works: after launch, a `simctl push` is **delivered** through
  the usernotifications pipeline to `com.aura.app` (it is dropped without auth).
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
- The simulator **rejects the App Group access group `group.com.aura.app`**
  (whether in `application-groups` *or* `keychain-access-groups`) **unless the
  App Group is provisioned for the signing team**. Manual `codesign` — ad-hoc
  *or* with a real Development identity — **cannot register an App Group**; the
  launch is denied. (A bare `get-task-allow`-only sign launches fine, isolating
  the App Group as the blocker.)
- `codesign` with a real identity also needs interactive keychain authorization
  (it fails headlessly with `errSecInternalComponent`).

The reliable path is therefore **Xcode automatic signing**: open
`src-tauri/gen/apple/aura-mobile-app.xcodeproj`, set your team under Signing &
Capabilities for **both** the app and the NSE target — Xcode registers
`group.com.aura.app` and provisions it (a **paid Apple Developer** capability;
`com.aura.app` may need a team-unique bundle id, kept in sync with the App Group
id in `PushKeyStore.swift` / `keychain.rs`). `apple/verify-nse.sh` automates the
build + signing + seed + push and, when the App Group is not yet provisioned,
detects the launch denial and prints this path. PASS = a notification reading
*"Aura / The agent finished replying."*

## Remaining M4 work (needs the external resources above)

1. **Device-token capture** — `registerForRemoteNotifications` is triggered, but
   the token arrives at `application:didRegisterForRemoteNotificationsWithDevice
   Token:` on the `UIApplicationDelegate`, which the Tauri/wry runtime owns.
   Capture it by adding that delegate method at runtime (objc2 `class_addMethod`
   on the live delegate, or a small Tauri iOS plugin), store the hex token, and
   feed it into `PairingRequest.apns_token` (currently `String::new()`). Only
   yields a *real* token on a signed build with `aps-environment` (dev account).
2. **`.p8` provisioning** — register the APNs auth key with the operator's push
   role (`remote-host/crates/push`) and the App ID's Push Notifications capability.
3. **Device enrollment** — exercise the live `.p8` send to a physical device
   (the simulator can't receive real APNs); confirm A→C `/register` and the
   completed-turn → `/notify` → APNs → NSE path end-to-end.
