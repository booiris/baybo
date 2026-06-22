# mobile phase-5 — Android

> **Status: planning (roadmap altitude).** Builds on the iOS phases;
> architecture reference [`mobile-remote-host.md`](../../mobile-remote-host.md).
> Platform expansion: a second app under `app/mobile/android`, reusing the shared
> protocol/crypto core and generalizing **C**'s push to a second provider (FCM).

## Goal

Bring the companion to Android with **maximum reuse** of what phases 1–4 built —
the shared Rust core and the blind protocol are platform-agnostic; only the push
provider and the platform-native shells differ.

## Scope

**In:**

1. **`app/mobile/android`** — a Tauri Android app reusing `aura-mobile-core`,
   `aura-wire`, and `aura-device-proto` **unchanged** (this is exactly why phase 1
   kept the core as a separate, FFI-free, host-testable crate). Lives in its own
   Cargo workspace like the iOS app.
2. **FCM push path in C.** `aura-remote-host`'s push generalizes from "APNs only"
   to a **provider trait** with APNs + **FCM** (Firebase Cloud Messaging) impls.
   C's store tracks `device_id → { provider, token, env }`. The gateway A's
   dispatcher is unchanged (it still POSTs an opaque encrypted-preview blob to C;
   C picks the provider by the device's registration).
3. **Android preview decryption.** The iOS NSE equivalent: an FCM **data message**
   handler that decrypts the preview (the same `aura-device-proto` AEAD framing +
   the cross-language fixture, now also consumed by Kotlin/`javax.crypto` or the
   Android keystore) and posts a local notification. Key storage uses the Android
   **Keystore** (the `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly` analogue).
4. **Multi-platform device registry + registration.** `device_pubkey`, pairing
   (SPAKE2 over rendezvous), Noise E2E, multi-gateway — all reused. The new field is
   the **push provider**, and it must thread the *whole* registration wire, not just
   C's store: phase 1's gateway-mediated registration payload generalizes from
   `{ apns_token, env, device_id }` to **`{ provider, token, env, device_id }`**
   end-to-end (P→A→C), so A tags each registration with the provider C selects on.
   (The phase-1 `apns_token`-shaped payload cannot express an FCM token.)

**Out:** anything that forks the protocol or the blind invariants — Android must be
a peer of iOS on the **same** A and C.

## Key decisions / approach (to confirm when scheduled)

- **Push abstraction shape + the FCM size truth.** A `PushProvider` trait in C with
  `Apns` + `Fcm`; the preview blob stays opaque to both (C still blind). FCM's hard
  limit is **4096 bytes for the *entire* message**, shared across *all* data keys —
  so after JSON key overhead the usable ciphertext budget is **smaller** than APNs'
  ~4 KB-for-the-`aps`-payload. Confirm the preview fits the FCM budget *separately*
  from APNs; a preview tuned for APNs may not fit FCM.
- **FCM background delivery is a reliability cliff, not a mechanics tweak.** The iOS
  "visible alert reliably launches the NSE" model does **not** transfer: a *data-only*
  FCM message (required so the *app*, not the system, decrypts and renders) does
  **not** reliably wake an app that is force-stopped or under Doze / App-Standby / OEM
  battery management, and high-priority is quota-throttled per app. Decrypt-in-handler
  may need a **hybrid** (a minimal visible `notification` block + data) or accepting
  that preview decryption is **best-effort** on Android.
- **Tauri Android parity.** Re-validate the native plugins (barcode scanner, push
  registration, Keystore write) on Android; the QR/pairing/Noise/pull paths are the
  webview → Tauri command → `aura-mobile-core` flow, unchanged.
- **Google service account** for FCM (the C-side analogue of the `.p8`): C holds it,
  stays blind, isolatable like the push binary.

## Dependencies

- iOS phases landed and stable (Android mirrors a proven design).
- `aura-mobile-core` / `aura-wire` / `aura-device-proto` reused as-is — any change
  needed for Android should be platform-neutral and benefit both.
- C push generalized to the provider trait (the only substantial C change).

## Landing slices

1. **C provider trait + FCM impl** (server-side, testable against an FCM sandbox).
2. **Android Tauri shell** (pairing → Noise → self-pull → render, reusing the core)
   — the receive-only equivalent of iOS phase 1, but the protocol is already built.
3. **Android preview decryption** (FCM data handler + Keystore + the shared fixture).
4. **Feature parity** with the iOS phase the project is at (send/UI from P2, etc.).

## Open questions

- FCM 4096-byte budget after key overhead vs. the preview; reliable background wake
  for data-only messages (Doze / force-stop / OEM throttling).
- How much of the Tauri webview UI is shared between iOS and Android vs. tweaked.
- Android key-at-rest class equivalent to `…AfterFirstUnlockThisDeviceOnly`.

## Related

- [phase1.md](phase1.md) — the FFI-free shared core + blind protocol Android reuses.
- [`mobile-remote-host.md`](../../mobile-remote-host.md) — C's push (now multi-provider),
  the device registry, the blind constraints Android must also honor.
