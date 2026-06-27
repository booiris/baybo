# Mobile companion (iOS)

The iOS companion app pairs a phone to a Baybo gateway and gives it two
user-facing features:

1. **Scan-to-connect (扫码连接)** — pair the phone to a gateway by scanning a QR
   code. A mutual-confirm handshake establishes a device identity; both the phone
   user and the operator confirm a code derived from the handshake, and the
   gateway is then reachable for chat.
2. **Remote notifications (远程通知)** — when an agent turn completes, the phone
   gets a push whose lock-screen preview is **end-to-end encrypted**: the gateway
   encrypts, the operator's remote host forwards ciphertext blind, and the phone
   decrypts locally in a Notification Service Extension (NSE).

The app is a **Tauri 2** shell (`app/mobile`) over a host-tested, FFI-free Rust
core (`baybo-mobile-core`); the protocol + crypto live in shared crates so the
phone and the gateway agree by construction.

> The pairing handshake's **security model** (why it is safe against a hostile
> relay) is its own document: [`mobile-pairing-security.md`](mobile-pairing-security.md).
> This file is the architecture/wiring reference.

## Roles

| Role | Who | Trust |
|---|---|---|
| **A** | the Baybo gateway (the user's `baybo` instance) | holds session data; encrypts previews; the only party that authenticates the device |
| **C** | the operator-run **remote host** (`remote-host/`) | **blind** relay + APNs sender; sees only ciphertext, never plaintext, keys, or the pairing secret |
| **P** | the phone (this app) | decrypts previews locally; holds its push key + Noise identity |

C is a **separate Cargo workspace** that deliberately depends on no `baybo-*`
crate — its `/notify` + `/register` payloads are a JSON contract
(`remote-host-protocol`), so the `.p8`-holding push role stays isolatable. One C
fronts many gateways, admitted by `instance_key`.

## 1:1 binding

The companion is a **1:1** relationship: a gateway binds **one** device and the
app binds **one** gateway. The gateway is single-user (one gateway = one user),
so the chain is *gateway ↔ user ↔ app*. A↔P is the only durable binding; C is a
shared blind relay and is **not** bound 1:1.

- **Gateway (A):** a partial unique index
  `idx_devices_one_approved ON devices(status) WHERE status='approved'`
  (`crates/storage/src/libsql/mod.rs`) admits exactly one approved row at a time —
  the device domain has **no `user_id`**, so this is a single global approved row.
  `DeviceStore::create_replacing_approved` (`crates/store/src/device.rs`, libsql
  impl in `.../libsql/device.rs`) revokes every approved row and inserts the new
  one in one `BEGIN IMMEDIATE` transaction; re-pairing the **same** device id
  upserts in place, a **different** device supersedes the prior binding (revoked,
  kept for audit). `DevicePairingService::complete` calls it, so the swap happens
  only when the new pairing finalizes — an abandoned pairing leaves the existing
  binding live.
- **Operator consent:** `baybo device pair` prints any current binding and asks
  before minting a slot (defaults to **no**).
- **App (P):** one fixed keychain account (`baybo.paired-gateway`) holds the
  single `PairedRecord`; the UI offers **Replace** (re-pair, overwrite on success)
  and **Forget** (`forget_pairing` — drops the record + push key). The app's
  Noise static identity lives under its own account (`baybo.device-identity`) and
  is **stable** across re-pairings, so the derived `device_id` (`ios-<pubkey[..8]>`)
  is stable; *Forget* deliberately keeps it.

## Components

- `crates/wire` — the `Frame` / `Message` WS wire types (shared with the web channel).
- `crates/device-proto` — the device protocol + crypto:
  - `psk_pair` — the pairing **XXpsk0** state machine over `snow` (`PskHandshake` /
    `PskTransport`), the `PairingSecret` newtype, and the canonical prologue
    builder (see the security doc). SPAKE2 (`pake.rs`) is **deleted**.
  - `noise` — the post-pairing **Noise IK** content transport over `snow`, plus
    `write_chunked` / `FrameReassembler` (a length-prefixed plaintext stream that
    chunks a `Frame` past the Noise ~64 KiB per-message ceiling).
  - `aead` — ChaCha20-Poly1305 preview framing; `kdf` — HKDF-SHA256 → `channel`
    bindings + a 32-byte `push_key` + the confirmation code; `pairing` —
    `PairFrame` / `DeviceHello` / `GatewayWelcome` / `ApnsEnv`; `fixtures` — the
    pinned cross-language AEAD vector.
- `crates/pairing` — `DevicePairingService` (`mint` / `complete` / slot lookup +
  operator/device decisions).
- `crates/gateway` — A-side: the pairing host leg + `drive()` orchestration
  (`channel/device_pair.rs`, `channel/relay_pair.rs`), the device store, the
  **content session** responder (`channel/device_content.rs`), the relay-content
  control manager (`channel/relay_content.rs`), and the push dispatcher
  (`push/mod.rs`).
- `remote-host/` (C, separate workspace):
  - `crates/protocol` — `remote-host-protocol`, the wire contract (route paths,
    `x-instance-key`, `/notify` + `/register` bodies, `ControlHello` /
    `ControlSignal`, `ApnsEnv`, URL builders); path-depended across the boundary.
  - `crates/relay` — the blind byte-pipe `RelayBroker` + the WS rendezvous/content
    server; `crates/admission` — the hot-reloaded `instance_key` allow-list;
    `crates/push` — APNs (HTTP/2 sender, ES256 `.p8` JWT, `/notify` + `/register`);
    `crates/server` — the single binary serving relay + push on one listener.
- `app/mobile/core` (`baybo-mobile-core`) — P-side: `PairingClient`,
  `ContentSession` (Noise self-pull), and the direct-first/relay-fallback connect
  policy. No FFI, no platform APIs — host-unit-testable.
- `app/mobile/src-tauri` — the Tauri shell: the `pair` / `forget_pairing`
  commands, push-key keychain persistence (`keychain.rs`), and push registration
  (`push_register.rs`).
- `app/mobile/apple/NotificationExtension` — the NSE (`NotificationService.swift`,
  `PushKeyStore.swift`), plus `verify-crypto.swift` and `verify-nse.sh`.

## Wire & crypto contracts

### Pairing (`PairFrame` over the relay)

`Noise_XXpsk0_25519_ChaChaPoly_SHA256`, app = initiator, gateway = responder, run
over C's blind pairing rendezvous (`/pair/host/{rendezvous_id}` ⇄
`/pair/join/{rendezvous_id}`):

```
P → A   Hello { rendezvous_id, msg=e }       # XXpsk0 msg1 (psk pre-mixed)
A → P   HandshakeReply { msg }               # msg2 (e, ee, s, es) — gw static in-band
P → A   HandshakeFinal { msg }               # msg3 (s, se); payload = DeviceHello body
        ── both sides in transport mode, share the handshake hash h ──
P → A   Sealed( DeviceConfirm )              # phone user's decision (transport msg)
A → P   Sealed( GatewayWelcome )             # sent only after the operator confirms too
                                             #   — active auth_token, gw static, relay node
```

Both ends derive the same keys from the Noise handshake hash `h` via HKDF: the
**confirmation code** (a short human-comparable number) and the 32-byte
**`push_key`**. The QR carries the `rendezvous_id` (public) **and** a 256-bit
`secret` used as the XXpsk0 PSK that C never sees. The full threat model,
prologue binding, and secret hygiene are in
[`mobile-pairing-security.md`](mobile-pairing-security.md). The QR payload is
`baybo://pair?h={endpoint}&r={rendezvous_id}&s={secret}&k={instance_key}`
(`crates/cli/src/commands/device.rs`).

### Content session (post-pairing chat)

After pairing the device holds A's static Noise key and A holds the device's
static in the device row. The gateway runs a **Noise IK responder**
(`channel/device_content.rs` `run_content_session`): it reads the initiator's
first handshake message, authenticates the device by matching its static to an
*approved* row (`DeviceStore::lookup_approved_by_pubkey` — **no token rides the
content leg**), then Noise-wraps the *same* channel frame loop the TUI / web chat
use (`ChannelType::ios`, `Subscribe` + self-pull). Frames are chunked
(`write_chunked` / `FrameReassembler`). The transport is generic
(`BinarySink` / `BinarySource`), so the responder runs identically over a direct
WS or an outbound relay leg.

### Push preview (A encrypts → C relays blind → P's NSE decrypts)

```jsonc
// APNs payload C sends (enc/n copied verbatim from A's /notify body):
{ "aps": { "alert": { "title": "Baybo", "body": "New message" }, "mutable-content": 1 },
  "enc": "<base64 ciphertext||16-byte tag>",   // ChaCha20-Poly1305, empty AAD
  "n":   "<base64 12-byte nonce>",
  "kid": 0,                                     // key epoch (always 0 today)
  "bid": "<device_id>" }                        // binding id == device_id
```

`mutable-content: 1` wakes the NSE; it reads the 32-byte `push_key` for `bid`
from the **App Group keychain** (account `baybo.push-key.<bid>`, access group
`group.com.baybo.app`), ChaCha20-Poly1305-opens `enc` with nonce `n`, and
rewrites the visible `title`/`body`. On **any** failure it keeps the generic
"New message" placeholder — a bad key / wrong nonce / tamper are
indistinguishable. The Rust producer (`device_proto::aead`) and the Swift
consumer (CryptoKit `ChaChaPoly`) are pinned to one byte-exact vector in
`device_proto::fixtures`; a drift on either side fails a test.

**Registration:** after a successful handshake the gateway
(`HttpApnsRegistrar`, best-effort) POSTs `{instance_key, device_id, apns_token,
env}` to C's `/register`, so the phone never holds a C credential. The device
token is captured at launch by hooking the Tauri/wry-owned
`UIApplicationDelegate` (`push_register.rs`, `class_addMethod` on
`didRegisterForRemoteNotificationsWithDeviceToken`) and threaded (hex) + the
build's APNs env into `DeviceHello`.

## Reaching a NAT'd gateway (the relay)

A NAT'd gateway can't be dialed, so both pairing and post-pairing content ride
C's blind relay, which only matches two legs by key and copies opaque frames
(`remote-host/crates/relay/src/broker.rs`). Pairing is **relay-only**.

- **Content control plane** (`channel/relay_content.rs`): whenever an approved
  device exists the gateway holds a persistent outbound **control connection** to
  C (`/control`), presenting a persisted `relay_node_id`. When a phone arrives for
  that node id, C pushes `ControlSignal::OpenDataLeg`; the gateway dials
  `/content/host/{relay_key}` and runs the content responder over it, while C
  splices it to the phone's `/content/join/{node}` leg. The manager self-gates on
  the approved device row (idle when none), reading the relay URL + admission key
  recorded on the row at pairing — there is **no `relay`/`push` config block**. It
  is spawned + tracked under the shared `ShutdownSignal`
  (`baybo_gateway::spawn_relay_content`), owns its child tasks (the control pump +
  per-signal data legs), and drains on shutdown.
- **Relay hardening** (C side): a per-instance connection cap + per-gateway
  content-bandwidth throttle, a per-rendezvous `/pair/join` limiter, a parked-leg
  **TTL sweep** + a hard `MAX_PENDING_LEGS` ceiling, and a per-client-IP
  upgrade limiter ahead of admission (`RELAY_PER_IP_LIMIT`, default on, socket-peer
  keyed; behind a proxy disable it or resolve the real client via
  `RELAY_CLIENT_IP_HEADERS=cf-connecting-ip` — see `DEPLOY.md`). An admission reload
  that drops a key kicks that gateway's live connections, not just future ones.

## App lifecycle & persistence

The app stores its `PairedRecord` (auth token, gateway static key, routing
candidates, relay node id, Noise static secret) in the App Group keychain and
shows a "remembered" view on launch. The chat survives a background round-trip,
and the content session reconnects (with catch-up) on every iOS foreground.
`push_key` is persisted to the App Group keychain on a successful pair so the NSE
can read it on-device.

## Status & open items

Everything above is implemented and verified (cargo test/clippy on the root, iOS,
and remote-host workspaces; tsc on the frontend; `simctl push` through the
usernotifications pipeline; AEAD interop byte-exact). Real end-to-end APNs
delivery is verified on a paid Apple Developer account + a real device (the
simulator can't receive real APNs), and the App Group keychain / live NSE decrypt
work on a provisioned, code-signed build (Xcode automatic signing — see the
empirical boundary notes below).

**Open:** the `remote-host-dashboard` (a blind, metadata-only status router)
compiles but **nothing mounts it** — it isn't in `remote-host/crates/server/Cargo.toml`,
only a `TODO(dashboard)` in `main.rs`, with no real `MetadataProvider` impl and no
`DASHBOARD_ENABLE` gate. Wiring it is a deliberate follow-up.

### The signing boundary (empirical, iOS 26 simulator)

The App Group keychain (and thus the live NSE decrypt + real APNs) is gated on a
**provisioned, code-signed** build — an Apple boundary, not a code gap:

- An unsigned build's `SecItemAdd` returns `errSecMissingEntitlement (-34018)` —
  the code reaches the keychain; only the entitlement is unhonored.
- `get-task-allow` is mandatory to launch any re-signed build.
- The simulator rejects the App Group `group.com.baybo.app` unless it is
  provisioned for the signing team; manual `codesign` (ad-hoc or Development)
  cannot register an App Group.

The reliable path is **Xcode automatic signing** (set the team on both the app and
the NSE target — a paid Apple Developer capability). `apple/verify-nse.sh`
automates build + sign + seed + push and prints this path when the App Group
isn't provisioned. PASS = a notification reading *"Baybo / The agent finished
replying."*

## Testing

Each side is unit/integration-tested, and the spliced relay path is proven
end-to-end **across the workspace boundary**: `remote-host-relay` /
`remote-host-admission` are path-depended as gateway dev-dependencies (like
`remote-host-protocol`), and `crates/gateway/src/channel/relay_e2e.rs` boots a
real `remote-host` relay in-process to drive both paths through it —
`real_relay_splices_gateway_responder_and_mock_app` (the real Noise IK content
responder + a mock app) and `real_relay_pairs_gateway_and_mock_app` (the real
XXpsk0 pairing entry + a mock app landing an approved row). The AEAD interop is
pinned by `device_proto::fixtures` + `apple/verify-crypto.swift`.

## Deploying C

Canonical deploy doc: [`remote-host/DEPLOY.md`](../../remote-host/DEPLOY.md). The
short version: deploy the single `remote-host` binary (relay always on; push
mounts when `APNS_P8_PATH` is set), admit each gateway's `instance_key` in the
polled SQLite `admitted_instances` table, then pair the gateway against the host
with `baybo device pair --relay-url <host> --instance-key <admitted key>` — the
endpoint + key are baked into the QR and written to the device row, and the
gateway auto-starts its relay control connection + push from that row.

## Related

- [`mobile-pairing-security.md`](mobile-pairing-security.md) — the pairing threat
  model (hostile-relay MITM) and the XXpsk0 design.
- [`pairing.md`](pairing.md) — the **channel**-pairing gate (a *different*
  subsystem for sidecar-routed inbound; do not conflate with device pairing).
- [`gateway.md`](gateway.md) — the gateway crate that hosts the A-side routes,
  device store, content responder, and push dispatcher.
- [`remote-host/DEPLOY.md`](../../remote-host/DEPLOY.md) — operating C.
