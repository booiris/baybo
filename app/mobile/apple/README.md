# Baybo iOS — Apple-side artifacts (NSE)

The Rust client core (`app/mobile/core`, crate `baybo-mobile-core`) is the
FFI-free, host-tested protocol engine: pairing (scan-to-connect), the Noise
content session (self-pull), and the direct-first/relay-fallback connection
policy. It cross-compiles to `aarch64-apple-ios` unchanged.

This directory holds the **Apple-native pieces that require Xcode + the Tauri
iOS toolchain to build** — they are not part of any Cargo workspace. The one
implemented here is the **Notification Service Extension (NSE)**, which delivers
phase 1's *remote-notification* feature on the client.

## Why these live outside the Rust build

`UNNotificationServiceExtension` and `CryptoKit` are iOS frameworks; the NSE is a
separate app extension target inside the Xcode project that
`cargo tauri ios init` generates under `src-tauri/gen/apple/`. None of it can be
compiled or run on a Linux/macOS CI host without Xcode + the iOS SDK, so it is
kept here as reviewed, drop-in source rather than wired into `cargo test`.

**Correctness is verified, not hand-waved.** The NSE crypto is validated against
the exact cross-language vector in `device_proto::fixtures`
(`KEY`/`NONCE`/`CIPHERTEXT_HEX`/`PLAINTEXT`). The Rust test
`fixtures::pinned_vector_is_reproduced` guards the producer side; the bundled
`NotificationServiceTests.testDecryptsThePinnedFixture` guards this consumer
side against the identical bytes (run it in Xcode).

CryptoKit ships with macOS, so the crypto-critical decrypt path is also
verifiable **without Xcode** — `verify-crypto.swift` runs the same
`ChaChaPoly.SealedBox(combined:)` + `open` against the pinned fixture:

```
$ swift app/mobile/apple/verify-crypto.swift
PASS: CryptoKit decrypt matches the Rust AEAD fixture (title=Baybo, body=The agent finished replying.)
PASS: wrong key -> nil (placeholder kept)
```

This already passes on macOS — the Swift↔Rust AEAD interop is proven; only the
extension *packaging* (the `UNNotificationServiceExtension` target + keychain
entitlements) still needs Xcode.

## Files

| File | Role |
|---|---|
| `NotificationExtension/NotificationService.swift` | The NSE: decrypts the preview, rewrites the visible `title`/`body`; keeps the generic placeholder on any failure. |
| `NotificationExtension/PushKeyStore.swift` | Reads the per-binding push key from the shared App Group keychain (the host app writes it at pairing). |
| `NotificationExtension/NotificationServiceTests.swift` | Fixture-pinned interop test (run in Xcode). |

## The wire contract (already implemented on the server side)

The gateway (A) encrypts the preview and the push role (C) sends APNs this body
(`crates/gateway/src/push` + `remote-host/crates/push`):

```json
{
  "aps": { "alert": { "title": "Baybo", "body": "New message" }, "mutable-content": 1 },
  "enc": "<base64 ciphertext||tag>",
  "n":   "<base64 12-byte nonce>",
  "kid": "<key id>",
  "bid": "<binding/device id>"
}
```

`mutable-content: 1` fires this NSE; it decrypts `enc` (ChaCha20-Poly1305, key =
the 32-byte push key for `bid`, nonce = `n`, empty AAD) into
`{"title","body"}` and replaces the placeholder.

## Wiring it into the Tauri app (needs the iOS toolchain)

1. `cd app/mobile && cargo tauri ios init` — generates the Xcode project
   under `src-tauri/gen/apple/` wrapping `baybo-mobile-core`.
2. In Xcode, **File ▸ New ▸ Target ▸ Notification Service Extension**; replace
   its generated `NotificationService.swift` with the two `.swift` sources here
   and add `NotificationServiceTests.swift` to a test target.
3. **App Group + keychain sharing**: add an App Group (e.g.
   `group.com.baybo.app`) to *both* the app and the NSE targets, and a shared
   `keychain-access-groups` entitlement. Set `PushKeyStore.accessGroup` to it.
4. At pairing, the host app must write the derived 32-byte push key to the
   shared keychain at account `baybo.push-key.<bid>` (generic password,
   `kSecAttrAccessGroup` = the App Group) — the read side is `PushKeyStore`.
5. Enable the **Push Notifications** + **Background Modes ▸ Remote
   notifications** capabilities on the app target (M4: real APNs needs an Apple
   Developer account, a registered App ID, and the `.p8` provisioned into the
   push role).

## Still pending (hard external blockers)

- **Tauri app shell + any UI** — needs the iOS toolchain (`cargo tauri ios init`,
  Xcode, Simulator). Phase 1 may stay UI-less; the Rust commands wrap
  `baybo-mobile-core`.
- **M4 real APNs** — needs an Apple Developer account + a physical device to
  obtain a device token and exercise the live `.p8` send.
