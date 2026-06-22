# mobile phase-1 — iOS companion (Tauri): QR pairing + remote push

> **Status: planning.** Execution plan for the first shipping slice of the iOS
> companion. The architecture reference is [`mobile-remote-host.md`](../../mobile-remote-host.md);
> this doc is the *what-we-build-now* and *in-what-order*. Every `crates/…` /
> `app/mobile/ios/…` / `aura-remote-host/…` path is a **target**, not a fact,
> until the corresponding code lands.

This plan was settled in a design interview; it deliberately **overrides** several
choices in the original push design. Where it differs, this doc and
[`mobile-remote-host.md`](../../mobile-remote-host.md) win. The overrides are called
out in [Deltas from the original design](#deltas-from-the-original-design).

## Scope

Phase 1 ships an iOS companion that:

1. **Pairs by scanning a QR code** (SPAKE2 over a rendezvous), with explicit
   operator approval.
2. **Receives a real lock-screen preview when a UserChat turn completes**, blind
   end-to-end (the operator-run infra and Apple see only ciphertext).
3. **Opens to the full thread** by self-pulling over an end-to-end channel
   (direct-first, relay fallback) and rendering it.

It is **receive-only** (no composer / message-send) and the thread render is
deliberately minimal ("ugly is fine"). The value of phase 1 is proving the hard
chain — pairing + Noise E2E + self-pull + blind push + NSE decrypt — not polish.

**Out of phase 1 (deferred):** NAT hole-punching / iroh; push-key rotation;
message sending; the polished chat UI (folders, attachments, streaming render);
multi-tenant admission-key rotation + per-device rate caps on the remote host;
`app/mobile/android`.

## Terminology (read this first)

The word "gateway" collides. This plan fixes three names and uses **only** these:

| Name | Is | Was called |
|---|---|---|
| **A — aura gateway** | the user's backend: runs the agent, stores the transcript, holds `.p8`-encrypted-preview push keys, dials out to C | "the user's aura gateway" / "server" |
| **C — `aura-remote-host`** | operator-run shared infra: **push + relay + rendezvous + admission + dashboard**. Holds the `.p8`. Blind. | "push gateway" + "blind relay" (two services) |
| **P — the app** | the iOS Tauri app + its Notification Service Extension (NSE) | "iOS app + NSE" |

The **two E2E ends are P and A.** C and Apple are middle parties that must only
ever see ciphertext + routing metadata.

## Resolved decisions

### Framework & client shape

- **Tauri** app with a **simple webview UI in phase 1** (not headless). Native
  device capabilities are Swift **Tauri plugins**; the NSE is a separate native
  Xcode target.
- **UI tier:** pairing + per-gateway status + a **minimal thread view**. The E2E
  content channel + self-pull + render are **in** phase 1.
- **No UniFFI.** Because the app's logic runs in `src-tauri` (Rust, reached from
  the webview via Tauri commands) and the NSE decrypts with native CryptoKit,
  **no Swift consumes the Rust core** → no `xcframework`, no UniFFI codegen.
  `aura-mobile-core` is a plain Cargo dependency of `src-tauri`.
- **NSE decrypts with CryptoKit** (`ChaChaPoly`), links no Rust. The AEAD framing
  is pinned by a cross-language **test vector** emitted by `aura-device-proto`.

### Rust topology

- `crates/aura-wire` (**new**): the pure wire types (`Frame`, `Message`,
  `WireAttachment`, `TaskView`, …) + the `rmp-serde` codec, **extracted from
  `aura-channels`**. `aura-channels` re-exports them (server behaviour unchanged).
  This is required because `aura-channels → aura-tools → { libsql, axum, reqwest,
  rmcp, oauth2, libc }` — a chain that cannot and must not cross-compile to iOS.
  - The one non-mechanical part: relocate **`ApprovalDecision`** (a pure enum)
    from `aura-tools` to `aura-model`, so `aura-wire` stays lean. Update the
    ts-rs `ts-export` feature, the `sdks/channel-ts` codegen, and
    `scripts/check-ts-bindings.sh` accordingly.
- `crates/aura-device-proto` (**new**): the shared secure-channel + pairing
  protocol — SPAKE2, Noise (`snow`), the HKDF push-key derivation, the AEAD
  preview framing, the device-pairing wire messages, and the cross-language test
  vectors. **No FFI.** Both **A** (`crates/gateway`) and **P**
  (`aura-mobile-core`) depend on it → single source of truth for crypto + KDF
  labels + framing.
- **iOS-only crates** (`aura-mobile-core`, `src-tauri`) live in a **separate
  Cargo workspace** rooted at `app/mobile/ios/`, added to the root
  `[workspace] exclude`. They `path`-depend on the two shared crates. This keeps
  the root `cargo clippy --all --tests` zero-warning gate (CLAUDE.md) from ever
  trying to build an iOS/Tauri-configured crate for the host.
- `aura-mobile-core` stays a **separate crate** (not folded into `src-tauri`) so
  SPAKE2 / Noise / Frame / pull are host-unit-testable and reusable by a future
  `app/mobile/android`.

### Operator-run infra: one folder, isolatable `.p8`

- `aura-remote-host/` (**new**) is **one Cargo workspace** of normal host
  services (they pass the host clippy gate, unlike the iOS crates):
  - `common/` — admission keys, instance registry, rate-limit, hashed-id logging.
  - `admission/` — **"auth" = per-instance admission only** (machine-to-machine,
    Bitwarden installation-id model). **Never** device auth; **never** plaintext.
  - `push/` — **holds the `.p8`**: ES256 JWT, `POST /notify`, APNs send,
    `device_id → { apns_token, env }` store, `410`/`400` token pruning. **Built
    as its own binary** so it stays independently deployable.
  - `relay/` — the blind byte-pipe + SPAKE2 rendezvous brokering.
  - `dashboard/` — ops **metadata-only** UI (registered instances, hashed device
    bindings, rate/usage, APNs token health, relay stats). It is blind; it shows
    no conversation content.
- "One folder ≠ one blast radius": dev can build a single binary, but the
  `.p8`-holding `push` is separable so the high-exposure `relay` surface can't
  reach the key.

### Connectivity (redefined)

- **P and A both connect to C.** C decides whether they can talk directly; if so
  they go **P2P-direct** (C's data plane off the path); otherwise C **blind-
  relays** (always-relay fallback). C is the rendezvous + NAT-traversal
  coordinator (the Tailscale-DERP / iroh shape).
- **Phase-1 substrate = self-built blind relay + "direct-reachability probe
  first".** Content prefers a directly-reachable A (LAN / the user's Tailscale /
  the user's reverse proxy — candidate addresses handed to P at pairing over the
  K channel) and falls back to C's relay. This keeps the dominant
  "phone + server on the same network" case off C entirely.
- **Full NAT hole-punching + iroh are deferred** to a later phase. Phase-1 E2E is
  our own **Noise** (not iroh's node encryption), so the trust model stays clean.
- Consequence: **A must expose a non-loopback Noise endpoint** for the direct
  case (it currently binds `127.0.0.1:8888`). Safe because Noise authenticates
  end-to-end, but it is a new open surface.

### Pairing (SPAKE2 over rendezvous)

- **SPAKE2** (kept; not degenerated to a token-in-QR). The short code's strength
  is exactly used now that pairing crosses the untrusted C.
- QR payload `{ "rendezvous": "<C url>", "code": "<short C>" }`.
- A dedicated **token-free WS route `/v1/device/pair`** on A carries the PAKE
  blobs + the K-channel key exchange; it is gated by the PAKE itself. The
  `aura-device-proto` SPAKE2 logic is transport-agnostic, so the same code feeds
  the rendezvous in phase 1 and a relayed path later.
- Pending pairing state in a **libsql `DevicePairingStore`** that mirrors
  `ChannelPairingStore` (reuse `crates/pairing/src/code.rs` `generate_unique`,
  the TTL, and the janitor sweep); a new `DevicePairingService` drives the
  SPAKE2 + key-exchange flow.
- A's **static Noise key** lives in `SecretVault`; its public half is exchanged
  over the K channel — authenticated by the SPAKE2-derived K that C **cannot read
  or forge**, **not** gated by C — and is not in the QR. The code C is retained on
  the pending `devices` row as the human **approval handle**
  (`aura device approve <code>`).
- **Residual pairing risk:** a PAKE allows one online guess of the short code per
  run; mitigate with per-code rate-limit/lockout at C, single-use code + short TTL,
  and the explicit operator approve as a second gate. Noise alone does **not**
  exclude an operator MITM unless pairing itself wasn't MITM'd.
- **Explicit operator approval:** pairing lands a `pending` device whose
  `auth_token` is inert until `aura device approve`.

### Relay / A↔C connection

- When relay is enabled, **A holds a persistent outbound control WS to C** (a
  first for the gateway, which has only ever been a server). Data legs open on
  demand. `tokio-tungstenite` is already a workspace dep.
- **The relay is a pure blind byte-pipe.** `Frame` runs E2E inside Noise; C never
  sees it. One app connection = one Noise tunnel; session multiplexing is the
  `Frame::Subscribe` layer's job, not the relay's.
- Routing uses a **C-assigned, non-secret `relay_node_id`** handed to P at
  pairing (over K). **P never holds A's `instance_key`.** Noise authenticates the
  real endpoints, so a non-secret routing id is safe.
- Pairing-rendezvous and content-relay share **one** C primitive — "match two
  legs, copy bytes blind" — keyed by the pairing code vs. `relay_node_id`.
- Cost note: idle control connections are cheap and bounded (≈tens of KB each;
  Rust/tokio scales to 10⁵–10⁶); **content bandwidth is the open-ended cost** and
  is 100 % on C in always-relay — which is why direct-first (now) and hole-punch
  (later) exist.

### Push dispatcher (A-side)

- A new task subscribes to the `JobLifecycle` lifecycle-event broadcast bus
  (model it on `spawn_turn_state_projector`). Lives in `crates/gateway/src/push/`,
  started when `push.enabled`.
- **Extend `JobLifecycleEvent` with `kind: JobInputKind` and `shape: JobShape`**
  (additive; the projector ignores them) so the dispatcher filters **without**
  re-fetching the Job.
- **Filter: `shape == Turn && kind == UserChat`** (real user turns). This excludes
  `Cron`, `System` (background compression), `Spawned`, `SubagentNotification`,
  **and** `/compact` — a `UserChat`-input but `Maintenance`-shape job that filtering
  on `kind` alone would wrongly buzz. (`JobInputKind` = `{ UserChat, Cron, System,
  Spawned, SubagentNotification }` — no `Compression` variant; see
  `crates/job/src/kind.rs`.)
- Per terminal Turn+UserChat edge: `session_id → user_id`; list **approved** devices
  for the user; fetch the session's last assistant message; build a **short
  preview** (title + first ~200 chars, generic fallback for non-text, bounded so
  the ciphertext stays well under APNs 4 KB); **per-device AEAD-encrypt** with
  that device's push key (`SecretVault` `device.<device_id>.push_key`); `POST` to
  `push.gateway_url` (C).
- `collapse_id = "<bid>:<session_id>"` (bid = the per-binding `device_id`) so
  pushes from different gateways never coalesce.
- **Verify at implementation (read-after-write):** the lifecycle event publishes
  *after* the job store write (`persist_and_publish`), but the assistant **message**
  is a separate write with no ordering guarantee. A naive read can encrypt the
  **previous** turn's reply — a plausible-looking *wrong* lock-screen preview, worse
  than empty. Gate the preview on the message cursor (don't build until the
  session's last row is newer than this turn's start, or carry the message
  `ordinal` in the trigger); bounded retry (count + backoff); on expiry send the
  generic placeholder, never prior-turn text.

### Push payload, NSE, key sharing

- Push payload custom keys alongside `aps`: **`{ enc, n, kid, bid }`**.
  - `enc` = AEAD ciphertext+tag, `n` = fresh random 12-byte nonce, `kid` = key
    epoch (always `0` in phase 1; the field exists so rotation needs no payload
    change), `bid` = per-binding `device_id` (selects the key — see multi-gateway).
- AEAD = **ChaCha20-Poly1305**, 32-byte key, fresh random nonce per message, AAD
  fixed/empty. Format pinned in `aura-device-proto` + a fixture the NSE's Swift
  tests consume.
- **Static push key, no rotation in phase 1.** No ratchet (a ratchet would force
  app↔NSE ratchet-state sync across the process boundary).
- Key sharing: main app + NSE share an **App Group** + a **Keychain access
  group**. The push key is stored `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`
  (notifications arrive on a locked screen, so `WhenUnlocked` would fail there;
  `…ThisDeviceOnly` keeps it out of iCloud Keychain backups). **Conscious
  trade-off:** this is a hot, unlocked-class secret — its compromise reveals only
  the short **preview**, never the transcript (which needs the Noise static key, a
  stronger class), and is bounded by `kid` rotation.
- NSE: read `bid` → load `push_key[bid]` → decrypt with `kid` → rewrite
  `title`/`body`. **On any failure (missing/ malformed key, decrypt fail,
  timeout) fall back to a placeholder** and implement
  `serviceExtensionTimeWillExpire()`.

### Multi-gateway (one install ↔ N gateways)

- `device_id` is **per-binding unique** (not per-install). Each pairing →
  a distinct `(user_id, device_id)` row in that gateway's own `devices` table +
  its own push key `K_i`.
- The NSE selects the key by `bid` in the payload (= that binding's `device_id`).
  The Keychain holds multiple push keys keyed by `bid`.
- **APNs token registration is gateway-mediated.** P sends `{ apns_token, env,
  device_id }` to A over Noise at pairing; **A** registers `device_id → apns_token`
  with C using A's `instance_key`. P never holds a C credential.
- App UI grows a **paired-gateway list** and groups sessions per gateway;
  connections are per-gateway on demand.
- **Accepted metadata leak:** because one install has one APNs token, C can
  correlate a phone's multiple gateway bindings via the shared token. C still sees
  no content. Inherent to multi-gateway; acceptable under "blind = no plaintext".

## Crate / service / app layout

```
crates/aura-wire/                       # pure Frame/Message wire types + rmp codec (new; extracted)
crates/aura-device-proto/               # SPAKE2 + Noise + HKDF push key + AEAD framing + fixtures (new)
crates/model/                           # ApprovalDecision relocated here (edit)
crates/channels/                        # re-export wire types from aura-wire (edit)
crates/store/src/device.rs              # DeviceStore + DeviceRow; DevicePairingStore (new)
crates/storage/src/libsql/device.rs     # LibsqlDeviceStore + LibsqlDevicePairingStore (new)
crates/gateway/src/auth/channel.rs      # AuthedClient::Device variant (edit)
crates/gateway/src/api/device/          # /v1/device/pair WS + enroll/registration (new)
crates/gateway/src/push/                # push dispatcher: subscribe JobLifecycle → POST C (new)
crates/gateway/src/relay/               # outbound control WS to C + data legs + direct Noise endpoint (new)
crates/job/src/lifecycle.rs             # add kind: JobInputKind + shape: JobShape to JobLifecycleEvent (edit)
crates/cli/src/commands/device.rs       # aura device pair|approve|list|revoke (new)

aura-remote-host/                       # operator infra; one Cargo workspace of normal host services (pass host clippy)
  crates/common/                        #   admission keys, instance registry, rate-limit, hashed logging
  crates/admission/                     #   per-instance admission (machine-to-machine)
  crates/push/                          #   holds .p8; ES256 JWT; /notify; APNs; token store + pruning  (own binary)
  crates/relay/                         #   blind byte-pipe + SPAKE2 rendezvous brokering
  crates/dashboard/                     #   ops metadata-only UI backend
  web/                                  #   dashboard frontend (reuse web/ React+Vite+Tailwind stack)

app/mobile/ios/                         # SEPARATE Cargo workspace (root [workspace] exclude)
  Cargo.toml                            #   its own [workspace.dependencies]; path-deps the shared crates
  src-tauri/                            #   Tauri app crate; depends on aura-mobile-core (plain Cargo dep)
  core/   (aura-mobile-core)            #   client: SPAKE2/Noise/Frame/pull; host-testable; no FFI
  frontend/                             #   React 19 + Vite + Tailwind v4; reuses @aura/channel-sdk types
  plugins/                              #   Swift Tauri plugins: APNs registration, Keychain access-group write
  gen/apple/                            #   Tauri-generated Xcode project + the NSE target (hand-added)
```

> Dependency direction follows the repo convention (trait in `aura-store`, libsql
> impl in `aura-storage`, business logic in its own crate). `aura-remote-host`
> crates are normal root-workspace members (host services). The iOS crates are a
> separate workspace so the host clippy/test gate never compiles them.

## Milestone build order

`xcrun simctl push` injects a payload into the Simulator **locally** — no real
APNs, no token, no Apple account. So everything except the final real-delivery
hop is offline-testable, **including the NSE decrypt** (encrypt a payload with
`aura-device-proto`, drop a test push key into the Simulator's shared Keychain,
`simctl push` it). The Apple-account / real-device dependency is isolated to
**M4**.

| M | Deliverable | How it's tested | Apple acct? |
|---|---|---|---|
| **M0 — protocol foundation** | `aura-wire` extraction (+ `ApprovalDecision` relocation, ts-rs / channel-ts / check-ts-bindings updates); `aura-device-proto` (SPAKE2, Noise, AEAD framing, fixtures) | host unit tests: A↔P SPAKE2 handshake, Noise handshake, AEAD round-trip, cross-language fixture | ❌ offline |
| **M1 — gateway registry + pairing + E2E (direct)** | `devices` + `DevicePairingStore`/`Service`; `/v1/device/pair` WS; `AuthedClient::Device` (scoped to `/v1/chat/*` + channel-ws as `ChannelType::ios`); A static Noise key + non-loopback Noise endpoint; `aura device …` CLI; config | integration-tests driven by a **Rust test client** (consumes `aura-mobile-core`, no app yet): pair → Noise → `Frame::Subscribe` pull → decode | ❌ offline |
| **M2 — remote host C** | `aura-remote-host`: `admission`, `relay` + rendezvous, `push` (ES256 JWT signing; APNs send behind a seam), `dashboard`; A↔C control connection + data legs; push dispatcher (`kind`+`shape` fields, Turn&&UserChat filter, encrypt, POST) | full loop A↔C↔test-client; `/notify` against a **mock APNs**; JWT signing unit-tested | ❌ offline |
| **M3 — iOS app (Simulator)** | Tauri scaffold; reuse `aura-mobile-core`; Tauri commands (`start_pairing`, `get_status`, `pull_thread`, `register_push`); React UI (gateway list + status + minimal thread); plugins (barcode-scanner QR, Keychain; APNs-registration plugin written but token acquisition stubbed); NSE target | Simulator: real app pairs via C, pulls + renders; NSE decrypt validated via `simctl push` + a Keychain test key | ❌ offline |
| **M4 — real APNs / device** | entitlements + provisioning (Push, App Groups, NSE); APNs registration actually returns a token → gateway-mediated registration with C; C real-sends to `api.sandbox.push.apple.com` with the `.p8`; real-device end-to-end | real device: turn completes → C → APNs sandbox → NSE lock-screen preview → tap → self-pull | ✅ **only here** |

Dependency-sound: M0 underpins all crypto; **M1 needs no C** (pairing transport
stubbed in-process — `aura-device-proto` is transport-agnostic); M2 adds real C;
M3 adds the real app; M4 adds only the Apple layer. Protocol + integration risk
is front-loaded into the host-/Simulator-testable M0–M2. **M3 is the first full
demo and needs no Apple account; M4 is the only account/device milestone.**

**Test-harness handle:** the M1/M2 **Rust test client** (built on
`aura-mobile-core`) keeps protocol validation green in CI before the iOS
toolchain is in the loop.

## Configuration (A-side `aura.json`)

```jsonc
"push":   { "enabled": true, "gateway_url": "https://remote.aura.example", "instance_key": "…" },
"relay":  { "enabled": true, "base_url": "wss://remote.aura.example/r",     "instance_key": "…" },
"direct": { "enabled": true, "advertise": ["wss://aura.lan:8889", "wss://aura.tailnet.ts.net:8889"] }
```

A does **not** store the `.p8` — only `gateway_url` + `instance_key`. The
`apns.*` secrets live only in C's own store.

## Deltas from the original design

This plan overrides [`mobile-remote-host.md`](../../mobile-remote-host.md)'s
predecessor on:

1. **Two services → one `aura-remote-host`** (push + relay + rendezvous +
   admission + dashboard), with the `.p8`-holding `push` kept independently
   deployable.
2. **Connectivity:** the QR no longer encodes a directly-reachable gateway URL.
   P and A both connect to C, which coordinates; content is **direct-first with
   relay fallback**; the **relay/rendezvous is in phase 1** (it was "deferred").
3. **No UniFFI / xcframework.** Tauri runs the Rust core in `src-tauri`; the NSE
   uses CryptoKit. The shared client core is a plain Cargo crate.
4. **Tauri**, with a simple webview UI in phase 1 (the original assumed a native
   Swift app).
5. **Multi-gateway** in phase 1 (per-binding `device_id`, `bid`-keyed NSE key
   selection, gateway-mediated token registration).
6. **Push filter = real user turns** (`shape == Turn && kind == UserChat`; the
   original buzzed `SubagentNotification` and named a non-existent `Compression`
   kind — `/compact` is `UserChat`-input but `Maintenance`-shape).

## Related

- [README.md](README.md) — the phase roadmap (P1–P5).
- [`mobile-remote-host.md`](../../mobile-remote-host.md) — the architecture reference.
- [`modules/pairing.md`](../../modules/pairing.md) — the channel-pairing patterns
  (`generate_unique`, TTL, janitor, CLI) the device pairing reuses.
- [`modules/storage.md`](../../modules/storage.md) — the store-trait / libsql split.
- [`modules/gateway.md`](../../modules/gateway.md) — admin/channel auth, `/v1/chat/*`,
  `/v1/channel-ws`, the `Frame` protocol the app reuses.
- [`modules/security.md`](../../modules/security.md) — `SecretVault` for A's Noise
  static key + per-device push keys.
- `crates/job/src/lifecycle.rs` — the `JobLifecycle::Completed` broadcast the
  push dispatcher subscribes to.
