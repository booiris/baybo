# Mobile companion — remaining work

Pick-up notes for the iOS companion after the pairing + relay-pairing + persist
work landed on branch `feat/mobile-companion`. Read [`phase1.md`](phase1.md) for
the overall design and the A (gateway) / C (remote-host) / P (phone) roles.

## What already landed (for context)

- **Pairing**: Bluetooth-style mutual-confirm handshake (both the phone user and
  the operator confirm a code derived from the SPAKE2 secret; no separate
  `device approve` step; device rows are `Approved` from creation). `device pair`
  is interactive, label is optional (the device reports its own name), renders a
  terminal QR, default endpoint `wss://proxy.baybo.space`.
- **Relay pairing (C+A+P)**: the unified `remote-host` binary hosts the blind
  rendezvous (`/pair/host/{code}` — the gateway presents its `x-instance-key`,
  checked against the polled `admitted_instances` SQLite table; `/pair/join/{code}`
  for the app). The gateway's `channel/relay_pair.rs` host manager parks a leg per
  live slot; the app joins `/pair/join/<code>` when the QR carries `&relay=1`.
  `drive()` is transport-generic (`PairTransport`).
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
- **Multi-gateway persist**: the `PairedRecord` is stored under one fixed account
  (`baybo.paired-gateway`); pairing with a second gateway overwrites it. Key by
  device id + a "current" pointer if multi-gateway is needed (a product call).
- **Dashboard not wired in** — `remote-host-dashboard` (a blind, metadata-only
  status router: counts of admitted instances / device tokens / connected
  gateways / pending legs, never content) compiles as a workspace member but
  nothing mounts it. To enable: impl its `MetadataProvider` over the real
  push/relay components and `app.merge(remote_host_dashboard::router(provider))`
  in `remote-host/crates/server/src/main.rs` (see the `TODO(dashboard)` there),
  likely behind a `DASHBOARD_ENABLE` env gate.
- **PR #132** is closed; reopen or open a fresh PR when ready.

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
