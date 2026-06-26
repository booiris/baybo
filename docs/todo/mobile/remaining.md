# Mobile companion — remaining work

Pick-up notes for the iOS companion after the pairing + relay-pairing + persist
work landed on branch `feat/mobile-companion`. Read [`phase1.md`](phase1.md) for
the overall design and the A (gateway) / C (remote-host) / P (phone) roles.

## What already landed (for context)

- **Pairing**: Bluetooth-style mutual-confirm handshake (both the phone user and
  the operator confirm a code derived from the Noise handshake hash `h`; no
  separate `device approve` step; device rows are `Approved` from creation).
  `device pair` is interactive, renders a terminal QR, default endpoint
  `wss://proxy.baybo.space`. The
  former malicious-relay MITM is **closed** (TODO #6): the QR carries a public
  `rendezvous_id` (C's routing key) *and* a 256-bit `secret` used as the Noise
  **XXpsk0** PSK, which C never sees — so a hostile relay can't complete the
  handshake (MITM → DoS).
- **Relay pairing (C+A+P)**: the unified `remote-host` binary hosts the blind
  rendezvous (`/pair/host/{rendezvous_id}` — the gateway presents its
  `x-instance-key`, checked against the polled `admitted_instances` SQLite table;
  `/pair/join/{rendezvous_id}` for the app). The gateway's `channel/relay_pair.rs`
  host manager parks a leg per live slot (re-parking immediately on a PSK-auth
  failure so a leg-stealer can't grief); the app joins
  `/pair/join/<rendezvous_id>`. Pairing is relay-only. `drive()` is
  transport-generic (`PairTransport`).
- **Persist**: after pairing the app stores a `PairedRecord` (auth token,
  gateway static key, routing candidates, relay_node_id, Noise static secret) in
  the App Group keychain; `paired_user` + a React "remembered" view on launch.

Everything above is committed + verified (cargo test/clippy on the root, iOS, and
remote-host workspaces, tsc on the frontend).

## Later on this branch — remote-host SDK + hardening ✅

A second wave landed after the items above; none had an entry here before:

- **`remote-host-protocol` shared wire crate** (`remote-host/crates/protocol/`):
  the single source of truth for C's route paths, the `x-instance-key` header, the
  `/notify`+`/register` bodies, `ControlHello`/`ControlSignal`, `ApnsEnv`, and
  transport-free URL builders. Pure serde, zero baybo/transport deps; the server,
  gateway, and app all depend on it and deleted their hand-written mirrors.
- **DB-backed admission + hot-reload** (`remote-host/crates/server/src/admission_db.rs`,
  `crates/admission/`): the allow-list is a polled libsql `admitted_instances`
  table (the old `RELAY_INSTANCE_KEYS` env is gone). Edit it out of band with
  `sqlite3`; changes apply within `ADMISSION_POLL_SECS` (default 30s), no restart.
- **Kick-on-revoke** (`remote-host/crates/relay/src/conns.rs`): a reload that drops
  a key closes that gateway's live relay connections (control + host legs), not
  just future ones — a `ConnectionRegistry` wired into the poll callback.
- **Single binary, single port**: `remote-host` serves relay + push on one listener
  by disjoint paths (`BIND_ADDR`, default `0.0.0.0:7777`), in-process rustls TLS via
  `TLS_CERT`/`TLS_KEY`. There is no separate `remote-host-relay` binary anymore —
  it's now just a library crate.
- **Push auto-on from the `.p8`**: relay is always on; push mounts only when
  `APNS_P8_PATH` is set (no `*_ENABLE` flags).
- **Content frame chunking** (`crates/device-proto/src/noise.rs`): `write_chunked`
  + `FrameReassembler` chunk a `Frame` past the Noise ~64 KiB per-message ceiling
  (a length-prefixed plaintext stream); both content paths use it. Closes §1's
  former "known limit".
- **ts-rs Tauri IPC types**: `PairChallenge`/`PairedSummary` generate to
  `app/mobile/src/generated/` from `app/mobile/core/src/pairing.rs`, drift-gated by
  `scripts/check-ts-bindings.sh` (no more hand-written TS mirrors for those).

---

## TODO

### 1. Direct content session — a paired device can chat ✅ done

Gateway `/v1/device/content` Noise-IK responder (+ the transport-generic
`FrameSink`/`FrameSource` seam in `channel/adapter.rs`) + Tauri content wiring +
chat UI. See `crates/gateway/src/channel/device_content.rs`,
`app/mobile/src-tauri/src/content.rs`, `app/mobile/src/App.tsx`. (Frames larger
than the Noise ~64 KiB ceiling are now chunked — see the SDK/hardening section
above.)

### 2. Content relay — content traffic for a NAT'd gateway ✅ done

A NAT'd gateway now serves content through the blind relay. The gateway holds a
persistent A→C control connection at boot (`channel/relay_content.rs` +
`relay/mod.rs` `connect_control`/`pump_control`), presenting a persisted
`relay_node_id` (`load_or_create_relay_node_id`) it also advertises in
`GatewayWelcome` (+ `relay_url`). C mounts `/control`, `/content/join/{node}`
(phone parks, C signals the gateway), `/content/host/{key}` (gateway data leg);
the existing `RelayBroker`/`pump_ws` splice the two legs blind
(`remote-host/crates/relay/src/serve.rs`). The gateway's content responder is
transport-generic (`device_content.rs` `BinarySink`/`BinarySource`), so it runs
the Noise IK responder over the outbound relay leg too — authenticating the
device by looking its static key up among approved rows
(`DeviceStore::lookup_approved_by_pubkey`, since the relay leg carries no token).
The app falls back via `connect_first` → `dial_relay`
(`{relay_url}/content/join/{node}`). Tested end-to-end on C
(`content_relay_splices_phone_and_gateway`) and on A
(`relay_path_resolves_device_by_pubkey_and_round_trips`).

### 3. Real APNs device token ✅ done

`push_register.rs` hooks the (Tauri/wry-owned) `UIApplicationDelegate` at launch
via `class_addMethod`, capturing the token iOS delivers to
`didRegisterForRemoteNotificationsWithDeviceToken` (and logging the failure
callback) into a process-global; `pair_begin` threads it (hex) + the build's
APNs env into `DeviceHello`. Empty until the async token lands — registration
fires at launch, so it's normally ready by pairing time, and a too-early pairing
falls back to the gateway's out-of-band re-registration. Compiles + clippy-clean
for host and `aarch64-apple-ios-sim`; real end-to-end delivery still needs #4
(paid account + device).

### 4. External blockers (need resources, not just code)

- **M4 real end-to-end APNs delivery**: paid Apple Developer account + a real
  device (the simulator can't receive real APNs).
- **App Group keychain / provisioned, code-signed build**: paid account + Xcode
  automatic signing, so the NSE can read the push key on-device (see the
  empirical boundary notes in `phase1.md`). Until then the live NSE decrypt path
  is unverifiable.

### 5. Hardening / smaller follow-ups

- **Relay parked-leg TTL** ✅ done — `RelayBroker` stamps each parked half with
  `parked_at` and reaps stale halves (TTL 120s) opportunistically on every
  `join` that parks, so a WS upgrade that fails *after* the synchronous
  `broker.join` (its `on_upgrade` cancel never runs) or a no-show gateway can't
  grow the pending map without bound. (`remote-host/crates/relay/src/broker.rs`,
  `sweep`.)
- **Relay flood / rate-limiting** — lower priority now: `/pair/join` uses
  `try_match` (never parks); `/content/join` parks only *after* `signal_open`
  succeeds for a currently-connected gateway, and that gateway's control channel
  is capped (32), so the park rate is already throttled; the TTL sweep bounds any
  residue. A dedicated per-IP limiter / hard pending-cap remains optional.
- **Graceful shutdown for the relay managers** — lower value now: the managers
  (`relay_pair`, `relay_content`) spawn detached child tasks (per-slot pairing
  drives, the content control pump), so a correct drain would track + abort them,
  and the gateway process exits moments after the shutdown signal regardless. C
  already self-heals from an abrupt gateway disconnect (the control idle-timeout
  + the broker TTL sweep above), so the residual benefit is small.
- **No automated cross-workspace e2e** for relay pairing/content (relay and
  gateway are separate Cargo workspaces); each side is unit/integration-tested,
  but the spliced path is only proven on deployment. A harness that boots
  `remote-host` + a test gateway + a mock app would close this.
- **Multi-gateway persist** — resolved by the **1:1 binding policy** (see
  [§7](#7-one-gateway--one-app-11-binding--done)): one app binds exactly one gateway,
  so the single fixed `PairedRecord` account is the design, not a limitation. The
  silent overwrite is gone — re-pairing is an explicit *Replace* (kept until the
  new pairing finishes) and there's an explicit *Forget* unpair. Multi-gateway is
  intentionally not supported.
- **Dashboard not wired in** — `remote-host-dashboard` (a blind, metadata-only
  status router: counts of admitted instances / device tokens / connected
  gateways / pending legs, never content) compiles as a workspace member but
  nothing mounts it. To enable: impl its `MetadataProvider` over the real
  push/relay components and `app.merge(remote_host_dashboard::router(provider))`
  in `remote-host/crates/server/src/main.rs` (see the `TODO(dashboard)` there),
  likely behind a `DASHBOARD_ENABLE` env gate.
- **PR #132** is closed; reopen or open a fresh PR when ready.

### 6. Pairing malicious-relay MITM (SECURITY) ✅ done

Closed per [`pairing-mitm-xxpsk.md`](pairing-mitm-xxpsk.md). The pairing value is
split into a public `rendezvous_id` (the only thing C sees — the broker key, the
`/pair/{host,join}/{rendezvous_id}` param, the slot/first-frame selector) and a
256-bit CSPRNG `secret` carried **only** in the QR. Pairing now runs
**`Noise_XXpsk0_25519_ChaChaPoly_SHA256`** (app = initiator, gateway = responder)
with the secret as the PSK; SPAKE2 (`device-proto/src/pake.rs`) and the `spake2`
dep are deleted. What landed:

- **`device-proto`** (`psk_pair.rs`): the XXpsk0 state machine over `snow`
  (`PskHandshake`/`PskTransport`), a 256-bit `PairingSecret` newtype (CSPRNG ctor
  + `from_bytes` only — no string ctor, `Zeroize`/redacted `Debug`), and a
  canonical versioned length-prefixed **prologue** binding
  `version ‖ rendezvous_id ‖ endpoint ‖ role-labels` (anti-splice /
  anti-cross-binding, role binding). `kdf.rs` derives the confirm code + push key
  from the handshake hash `h` (binds both statics; not grindable). `PairFrame`
  reshaped to `Hello{rendezvous_id,msg}` / `HandshakeReply` / `HandshakeFinal` /
  `Sealed{msg}` / `Reject`; the statics ride as XX tokens (dropped from
  `DeviceHello`/`GatewayWelcome`); transport msgs use snow's implicit nonce.
- **Secret hygiene**: the secret lives only in the in-memory `DevicePairingSlot`
  (zeroized on drop) — never a plaintext column, never the durable `DeviceRow`
  (which stores `rendezvous_id`), never `device list`, `GatewayWelcome`,
  `PairedSummary`, or a log. `device list`'s column + JSON key are `rendezvous_id`.
- **Stage 3 relay rename** + honest doc comments (C sees only the public
  rendezvous id) across `remote-host` protocol/serve/broker.
- **Hardening**: re-park immediately on PSK-auth failure (no backoff) + a
  per-rendezvous `/pair/join` rate limit + a warn metric on PSK failures; the
  app times out the handshake reply; the typeable QR fallback is removed (a render
  failure errors out asking for a wider terminal — never a shortened secret and
  never written to disk); the operator confirm prompt defaults to **no**.

Pairing is **relay-only** (no direct/LAN pairing route); the prologue binds
`endpoint`, which is what prevents cross-relay binding. Out of scope (unchanged):
`NKpsk0`/gateway-static-in-QR; dropping the shared `guest` admission key (the PSK
already defeats MITM, and the re-park + join rate-limit cover the residual
griefing). (The former "one-app-per-gateway policy" out-of-scope item is now
**done** — see [§7](#7-one-gateway--one-app-11-binding--done).)

---

### 7. One gateway ↔ one app (1:1 binding) ✅ done

The companion is a **1:1** relationship: a gateway binds **one** device, and the
app binds **one** gateway. Concretely the gateway is single-user (one gateway =
one user), and that user has at most one live device, so the whole chain is
*gateway ↔ user ↔ app*. Re-pairing **replaces** the old binding (newest wins),
always behind an explicit confirm — never a silent clobber.

**Gateway (A) side — at most one Approved device per user:**

- **DB backstop**: a partial unique index
  `idx_devices_one_approved_per_user ON devices(user_id) WHERE status='approved'`
  (`crates/storage/src/libsql/mod.rs`) makes a second live device structurally
  impossible. Revoked rows drop out of the partial index, so the audit trail of
  superseded devices is unbounded as before. The schema init runs an **idempotent
  reconciliation** right before building the index — keep each user's newest
  approved device, revoke older ones — so a DB that predates the policy (≥2
  approved rows for a user) converges to the invariant instead of failing index
  creation and bricking startup.
- **Atomic replace at finalize**: `DeviceStore::create_replacing_approved`
  (`crates/store/src/device.rs`, libsql impl in `.../libsql/device.rs`) revokes
  every still-approved row for the user and inserts the new one **in one
  `BEGIN IMMEDIATE` transaction** — no window with zero or two approved devices.
  `DevicePairingService::complete` calls it (not bare `create`), so the swap
  happens **only when the new pairing actually finalizes** (after both confirms).
  A new pairing that the phone or operator abandons leaves the existing binding
  untouched and live. The superseded `device_id`s are returned for the operator's
  "Replaced X" line and logged.
- **Operator consent up front**: `baybo device pair` calls
  `DevicePairingService::current_device(user)` before minting the slot; if a
  device is already bound it prints it and asks *"Pairing a new device will
  replace it once the new pairing completes. Continue?"* (defaults to **no**, per
  the fail-closed posture). Declining keeps the current pairing and never even
  shows a QR. On success the result reports what was replaced.

**App (P) side — one stored gateway, explicit replace + forget:**

- The `PairedRecord` already lives under one fixed keychain account
  (`baybo.paired-gateway`), so the app structurally holds one gateway. What
  changed is the UX: the old *Pair another* button silently overwrote it.
- **Replace** (re-pair): a confirm gate ("Replace this pairing? … the current one
  stays until the new pairing finishes") then drops to the scanner. The keychain
  record is overwritten **on success** (delete-then-add), so cancelling the scan
  leaves the existing binding intact — symmetric with the gateway's
  replace-at-finalize.
- **Forget** (unpair): a new `forget_pairing` Tauri command
  (`app/mobile/src-tauri/src/pairing.rs`) deletes the `PairedRecord` **and** the
  device's push key (`keychain::delete_paired_record` / `delete_push_key`) and
  returns to the scan screen, fully unpaired. Idempotent (`errSecItemNotFound`
  is a no-op). This is the explicit unbind affordance the connected screen now
  shows alongside *Open chat* and *Replace pairing*.
- **Stable `device_id`**: the app's long-term Noise static identity now lives
  under its own keychain account (`baybo.device-identity`,
  `keychain::store_device_identity` / `read_device_identity`) and is loaded at
  `pair_begin` instead of being minted fresh each pairing, so the derived
  `device_id` (`ios-<pubkey[..8]>`) is stable across re-pairings and launches.
  It is deliberately **not** cleared by *Forget* (which only drops the
  `PairedRecord` + push key), so re-pairing the same phone keeps the same id; a
  full identity reset would mean also deleting `baybo.device-identity`. On the
  gateway, `DeviceStore::create_replacing_approved` now **upserts** so the same
  `device_id` refreshes its row in place rather than colliding on the
  `(user_id, device_id)` primary key — a *different* device still supersedes the
  prior binding (revoked, kept for audit).

**Note — pushes already targeted the single device**: the dispatcher fans over
`list_for_user(user, Approved)`; with the invariant that's a one-element loop,
so no change was needed there (kept as a loop, harmless and forward-compatible).

---

## Deploying what's built

Canonical, kept-current deploy doc: [`remote-host/DEPLOY.md`](../../../remote-host/DEPLOY.md).
The short version:

1. Deploy the unified `remote-host` binary (see `remote-host/docker-compose.yml`).
   It serves relay + push on **one** listener, split by route path. Env:
   `BIND_ADDR` (default `0.0.0.0:7777`; set `PORT=443` in compose for a port-less
   `wss://host`), `ADMISSION_DB_PATH` (default `/data/admission.db`),
   `ADMISSION_POLL_SECS` (default 30). Set `TLS_CERT`+`TLS_KEY` to serve `wss/https`
   directly (in-process rustls), or front it with a TLS terminator. Push turns on
   automatically once `APNS_P8_PATH` (+ `APNS_KEY_ID`/`TEAM_ID`/`BUNDLE_ID`) is set.
2. Admit each gateway's `instance_key` in the SQLite table (polled, no restart):

   ```sh
   sqlite3 ./data/admission.db \
     "INSERT INTO admitted_instances(instance_key, label) VALUES('<key>','my gateway');"
   ```

   Removing a row revokes it within one poll and kicks its live connections.
3. Gateway `baybo.json` (both URLs resolve to the one `remote-host` listener):

   ```jsonc
   "push":  { "enabled": true, "gateway_url": "https://c.example.com", "instance_key": "<admitted key>" },
   "relay": { "enabled": true, "url": "wss://c.example.com",            "instance_key": "<admitted key>" }
   ```
