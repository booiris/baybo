# Baybo iOS — Apple-side artifacts (NSE)

The Rust client core (`app/mobile/core`, crate `baybo-mobile-core`) is the
FFI-free, host-tested protocol engine: pairing (scan-to-connect), the Noise
content session (self-pull), and blob legs. It cross-compiles to
`aarch64-apple-ios` unchanged.

This directory holds the **Apple-native pieces that require Xcode + the Tauri
iOS toolchain to build**. The main Tauri app and React UI live under
`app/mobile/src-tauri` and `app/mobile/src`; this directory contains the
Notification Service Extension (NSE) source and Apple-side verification helpers.

## Why these live outside the Rust build

`UNNotificationServiceExtension` and `CryptoKit` are iOS frameworks; the NSE is a
separate app extension target inside the Xcode project that
`cargo tauri ios init` generates under `src-tauri/gen/apple/`. The generated
`project.yml` includes the extension target and points its Swift sources back at
this directory. None of the extension target can be compiled or run on a
Linux/macOS CI host without Xcode + the iOS SDK, so only the byte-level crypto
contract is covered by normal Rust tests.

The NSE crypto is validated against the exact cross-language vector in
`device_proto::fixtures` (`KEY`/`NONCE`/`CIPHERTEXT_HEX`/`PLAINTEXT`). The Rust
test `fixtures::pinned_vector_is_reproduced` guards the producer side; the
bundled `NotificationServiceTests.testDecryptsThePinnedFixture` guards this
consumer side in Xcode.

CryptoKit ships with macOS, so the crypto-critical decrypt path is also
verifiable **without Xcode** — `verify-crypto.swift` runs the same
`ChaChaPoly.SealedBox(combined:)` + `open` against the pinned fixture:

```
$ swift app/mobile/apple/verify-crypto.swift
PASS: CryptoKit decrypt matches the Rust AEAD fixture (title=Baybo, body=The agent finished replying.)
PASS: wrong key -> nil (placeholder kept)
```

This checks Swift/Rust AEAD interop without exercising extension packaging,
entitlements, or APNs delivery.

## Files

| File | Role |
|---|---|
| `NotificationExtension/NotificationService.swift` | The NSE: decrypts the preview, rewrites the visible `title`/`body`; keeps the generic placeholder on any failure. |
| `NotificationExtension/PushKeyStore.swift` | Reads the per-binding push key from the shared App Group keychain (the host app writes it at pairing). |
| `NotificationExtension/NotificationServiceTests.swift` | Fixture-pinned interop test (run in Xcode). |
| `verify-crypto.swift` | Runs the fixture decrypt path on macOS with CryptoKit. |
| `verify-nse.sh` | Builds/signs the generated iOS project, seeds the keychain, and sends a `simctl push` payload. |

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

## Xcode wiring

`app/mobile/src-tauri/gen/apple/project.yml` is the reproducible source of truth
for the generated project:

- the app target embeds `NotificationExtension.appex`;
- both targets carry `group.com.baybo.app` as the shared app group;
- the app target has `aps-environment: development`;
- the app links `Security.framework`; the extension links
  `UserNotifications.framework`.

For a signed build, set an Apple Developer team on both targets and provision
the App Group, matching keychain access group used by `PushKeyStore.accessGroup`,
and Push Notifications capabilities. Real APNs still requires a physical device
and an APNs `.p8` configured on `remote-host`; the simulator path uses
`simctl push`.
