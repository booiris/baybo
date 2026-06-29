# Mobile companion (iOS)

The iOS companion app pairs a phone to a Baybo gateway and gives it two
user-facing features:

1. **Scan-to-connect** — pair the phone to a gateway by scanning a QR code. A
   mutual-confirm handshake establishes a device identity; both the phone user and
   the operator confirm a code derived from the handshake, and the gateway is then
   reachable for chat.
2. **Remote notifications** — when a real user-chat turn completes successfully,
   the phone gets a push whose lock-screen preview is **end-to-end encrypted**:
   the gateway encrypts, the operator's remote host forwards ciphertext blind,
   and the phone decrypts locally in a Notification Service Extension (NSE).

The app is a **Tauri 2** shell (`app/mobile`) over a host-tested, FFI-free Rust
core (`baybo-mobile-core`); the protocol + crypto live in shared crates so the
phone and the gateway agree by construction.

> The pairing handshake's **security model** (why it is safe against a hostile
> relay) is its own document: [`pairing-security.md`](pairing-security.md).
> The scan-to-pair bootstrap and relay + push threat model are documented in
> [`relay-push-security.md`](relay-push-security.md). This file is the
> architecture/wiring reference.

## Roles

| Role | Who | Trust |
|---|---|---|
| **A** | the Baybo gateway (the user's `baybo` instance) | holds session data; encrypts previews; the only party that authenticates the device |
| **C** | the operator-run **remote host** (`remote-host/`) | **blind** relay + APNs sender; sees only ciphertext, never plaintext, keys, or the pairing secret |
| **P** | the phone (this app) | decrypts previews locally; holds its push key + Noise identity |

C is a **separate Cargo workspace** that deliberately depends on no `baybo-*`
crate — its `/notify` + `/register` payloads are a JSON contract
(`remote-host-protocol`), so the `.p8`-holding push role stays isolatable. One C is
a **multi-tenant** host fronting many, possibly mutually-distrusting gateways. The
`remote_api_key` each relay leg presents in the `x-remote-api-key` dial header is
C's relay tenant key: it gates relay admission, connection caps, and bandwidth
quotas. Push is deliberately **keyless** at the HTTP caller layer; `/register` and
`/notify` are authorized by the device→gateway Ed25519 delegation chain instead.
C has **no "account" abstraction** and no `account_id`; it knows only relay
`remote_api_key`s and their limits. Who owns or bills a key is a **control-plane**
concern C never sees (`billing_account → {remote_api_key…}`, N:1); a leaked key is
rotated by re-issuing under the same billing account, relay-agnostic. A key is one
of two **tiers**: `guest` (auto-issued, shared trial key, low default limits,
GC-able) or `registered` (control-plane-provisioned, explicit per-row limits, no
TTL).

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
    builder (see the security doc).
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
    the relay `x-remote-api-key` header, keyless `/notify` + `/register` bodies,
    `ControlHello` / `ControlSignal`, `ApnsEnv`, URL builders); path-depended
    across the boundary.
  - `crates/relay` — the blind byte-pipe `RelayBroker` + the WS rendezvous/content
    server; `crates/admission` — the hot-reloaded `remote_api_key` allow-list;
    `crates/push` — APNs (HTTP/2 sender, ES256 `.p8` JWT, `/notify` + `/register`);
    `crates/server` — the single binary serving relay + push on one listener.
- `app/mobile/core` (`baybo-mobile-core`) — P-side: `PairingClient`,
  `ContentSession` (Noise self-pull), and the blob-leg client. No FFI, no
  platform APIs — host-unit-testable; the Tauri shell owns relay WebSocket and
  filesystem/keychain integration.
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
P → A   Sealed( DeviceDelegation )           # authorizes A's push signing key for this device
```

Both ends derive the same keys from the Noise handshake hash `h` via HKDF: the
**confirmation code** (a short human-comparable number) and the 32-byte
**`push_key`**. The QR carries the `rendezvous_id` (public) **and** a 256-bit
`secret` used as the XXpsk0 PSK that C never sees. The full threat model,
prologue binding, and secret hygiene are in
[`pairing-security.md`](pairing-security.md). The QR payload is
`baybo://pair?h={endpoint}&r={rendezvous_id}&s={secret}&k={remote_api_key}`
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
(`BinarySink` / `BinarySource`), so the responder can be tested in memory and
runs in production over the outbound relay data leg.

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
from the shared keychain access group (account `baybo.push-key.<bid>`, access
group `$(AppIdentifierPrefix)com.baybo.app`, expanded by signing and mirrored
into the app/NSE code through build-time configuration), ChaCha20-Poly1305-opens
`enc` with nonce `n`, and
rewrites the visible `title`/`body`. On **any** failure it keeps the generic
"New message" placeholder — a bad key / wrong nonce / tamper are
indistinguishable. The Rust producer (`device_proto::aead`) and the Swift
consumer (CryptoKit `ChaChaPoly`) are pinned to one byte-exact vector in
`device_proto::fixtures`; a drift on either side fails a test.

**Registration:** if the APNs token is available during pairing, P threads it
(hex) plus the build's APNs env into `DeviceHello`, and A persists that
registration material in its vault. A advertises its gateway push public key in
`GatewayWelcome`; P returns a sealed `DeviceDelegation` that authorizes that key
for this device. Before the first push to a device in a gateway run, A
best-effort POSTs a signed `/register` carrying `{device_id, apns_token, env,
gateway_pubkey, delegation, sig, counter}` from the persisted material, so a
restarted/pruned C can recover before `/notify`. If iOS delivers or rotates the
APNs token after pairing, P sends it to A over the sealed content channel
(`Frame::UpdateApnsToken`) on every connect; the gateway persists it and
re-registers on the next push. The phone never POSTs C's `/register` directly and
never holds the APNs `.p8` or any push provider credential; it only holds its APNs
device token, relay admission key, device identity, and `push_key`. The token is
captured at launch by hooking the Tauri/wry-owned `UIApplicationDelegate`
(`push_register.rs`, `class_addMethod` on
`didRegisterForRemoteNotificationsWithDeviceToken`).

**Direct-mode push (no pairing):** the direct transport has no pairing handshake,
so it provisions the *same* binding over the admin-token REST surface instead.
`direct_push_register` (`src-tauri/src/direct/push.rs`) reuses the phone's stable
Ed25519 identity, generates a `push_key` it stores in the shared App Group
keychain for its NSE, fetches the gateway push key via `GET /v1/push/params`,
signs the same delegation, and `POST /v1/push/register`s it (admin Bearer). The
gateway verifies the delegation and persists a **web push binding**
(`crates/gateway/src/push/web.rs`), which the dispatcher fans out to alongside
device rows — so C and the NSE are unchanged. The remote-host endpoint comes from
the gateway `[push]` config (`baybo_config::PushConfig`), not a QR; without it the
gateway reports push unavailable and direct chat stays foreground-only. The
`push_key` rides TLS + the admin token rather than a Noise handshake — a weaker
trust model detailed in
[`relay-push-security.md`](relay-push-security.md#direct-mode-push-web-identity).

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
- **Device dedup is gateway-only** (`channel/state.rs` `LegDedup`): the relay is
  **device-blind** — Noise runs *after* C splices the two legs, so C never learns
  `device_id` and cannot dedup. Instead, each content leg is handed its own
  `AbortHandle`; once its handshake resolves the `device_id` it registers in
  `WsChannelState.device_leg_registry` (`device_id → AbortHandle`) and **aborts the
  stale predecessor** for that device (e.g. a half-open leg from a prior foreground
  reconnect). `DashMap::insert` is atomic, so two legs racing for one `device_id`
  leave exactly one survivor; one gateway = one app, so the map holds ~one entry.
- **Remote-host hardening** (C side) — relay abuse controls are keyed on
  `remote_api_key`, resolved through one shared seam
  (`Admission::resolve(remote_api_key) -> Admit{Ok|Unknown|Expired}`, which applies
  the guest-tier defaults and the expiry check). Push does not call admission:
  `/register` + `/notify` are keyless and rely on the device delegation chain,
  per-device notify rate limits, a bounded device-token store, and the shared
  per-source-IP request backstop:
  - a **single-level connection cap** — a per-`remote_api_key` ceiling over all its
    relay legs (fallback `MAX_CONNS_PER_REMOTE_API_KEY_FALLBACK`, per-row
    `max_conns` override; the one control leg is exempt so a gateway at its cap can
    still reconnect control). There is **no** per-server connection sub-cap — a
    gateway holds too few legs (~one control + ~one data) for one to bind.
  - a **two-level, AND-enforced content-bandwidth throttle**: a per-`remote_api_key`
    ceiling (`max_bps`, the cross-tenant wall) **and** a per-`(remote_api_key,
    server)` sub-cap (`per_server_max_bps`, so one of a tenant's own gateways can't
    starve the others). Every byte debits **both** buckets and owes the larger debt;
    enforcement is *throttle, not drop* (TCP backpressure paces the sender). Per-row
    overrides are in `DEPLOY.md`; the guest-tier defaults are the columns on the
    reserved `guest` template row (`GUEST_TEMPLATE_KEY`) — a guest row's NULL column
    inherits it — falling back per-column to the `GUEST_*` consts in
    `remote-host/crates/admission/src/lib.rs` when that row is absent or also NULL.
  - a per-rendezvous `/pair/join` limiter, parked-leg TTL cleanup + a hard
    `MAX_PENDING_LEGS` ceiling, and an always-on per-client-IP request limiter
    ahead of relay admission and push body parsing. Client-IP resolution is shared
    (`CLIENT_IP_HEADERS`), while relay and push each have their own rate / burst /
    bucket-map knobs (`RELAY_IP_*`, `PUSH_IP_*` — see `DEPLOY.md`).
  - an admission reload that drops a key **kicks that key's live connections and
    forgets all its bandwidth buckets**, not just refusing future dials. Guest rows
    may carry an `expires_at`; expired guests are filtered on load, and the durable
    `gc_expired_guests` sweep is not mounted today.

## App lifecycle & persistence

The app stores its `PairedRecord` (auth token, gateway static key, routing
candidates, relay node id, Noise static secret) in the App Group keychain and
shows a "remembered" view on launch. The chat survives a background round-trip,
and the content session reconnects (with catch-up) on every iOS foreground. It
also reconnects on its own when a live leg drops mid-session: the Rust pump emits
a `content-disconnected` event on any unsolicited exit (socket close, the
inbound-liveness lapse, a remote-host restart) — but not on a deliberate
reconnect/disconnect, which aborts the task first — and the webview retries on a
short backoff, so chat recovers without waiting for the next foreground. A failed
dial backs off and retries the same way instead of stranding on "Connect failed".
`push_key` is persisted to the App Group keychain on a successful pair so the NSE
can read it on-device.

## Attachments

Mobile attachments use dedicated relay blob legs rather than the chat leg. The
blob path, token gate, bandwidth class, and upload quota are documented in
[`blob-transfer.md`](blob-transfer.md).

## Status & open items

The pairing/content/push path, relay E2E tests, AEAD interop vector, and mobile
UI are implemented in this branch. Real APNs delivery and App Group keychain
reads require a provisioned, code-signed build; the simulator can exercise
`simctl push` and the NSE path but cannot receive live APNs.

**Open:** `remote-host-dashboard` is not mounted by the `remote-host` server.

### The signing boundary (empirical, iOS 26 simulator)

The shared keychain access group (and thus the live NSE decrypt + real APNs) is
gated on a **provisioned, code-signed** build — an Apple boundary, not a code
gap:

- An unsigned build's `SecItemAdd` returns `errSecMissingEntitlement (-34018)` —
  the code reaches the keychain; only the entitlement is unhonored.
- `get-task-allow` is mandatory to launch any re-signed build.
- The simulator rejects unprovisioned App Group / Keychain Sharing entitlements;
  manual `codesign` (ad-hoc or Development) cannot register that capability.

The reliable path is **Xcode automatic signing** (set the team on both the app and
the NSE target — a paid Apple Developer capability). `apple/verify-nse.sh`
automates build + sign + seed + push and prints this path when the App Group
isn't provisioned. PASS = a notification reading *"Baybo / The agent finished
replying."*

## Testing

Each side is unit/integration-tested, and the spliced relay path is exercised
end-to-end across the workspace boundary: `remote-host-relay` /
`remote-host-admission` are path-depended as gateway dev-dependencies (like
`remote-host-protocol`), and `crates/gateway/src/channel/relay_e2e.rs` boots a
real `remote-host` relay in-process to drive both paths through it:
`real_relay_splices_gateway_responder_and_mock_app` (the real Noise IK content
responder + a mock app) and `real_relay_pairs_gateway_and_mock_app` (the real
XXpsk0 pairing entry + a mock app landing an approved row). The AEAD interop is
pinned by `device_proto::fixtures` + `apple/verify-crypto.swift`.

## Deploying C

Canonical deploy doc: [`remote-host/DEPLOY.md`](../../../remote-host/DEPLOY.md). The
short version: deploy the single `remote-host` binary (relay always on; push
mounts when `APNS_P8_HOST_PATH` is set in the Compose `.env`), admit each
gateway's relay `remote_api_key` in the polled SQLite `remote_api_keys` table,
then pair the gateway against the host with
`baybo device pair --relay-url <host> --remote-api-key <admitted key>` — the
endpoint + relay key are baked into the QR and written to the device row, and the
gateway auto-starts its relay control connection plus keyless push from that row.

## Related

- [`pairing-security.md`](pairing-security.md) — the pairing threat
  model (hostile-relay MITM) and the XXpsk0 design.
- [`relay-push-security.md`](relay-push-security.md) — scan-to-pair,
  relay, and push security, including remote-host transparency and boundaries.
- [`blob-transfer.md`](blob-transfer.md) — dedicated relay blob
  legs for mobile attachments.
- [`pairing.md`](../pairing.md) — the **channel**-pairing gate (a *different*
  subsystem for sidecar-routed inbound; do not conflate with device pairing).
- [`gateway.md`](../gateway.md) — the gateway crate that hosts the A-side routes,
  device store, content responder, and push dispatcher.
- [`remote-host/DEPLOY.md`](../../../remote-host/DEPLOY.md) — operating C.
