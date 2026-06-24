# Mobile companion — remaining work

Pick-up notes for the iOS companion after the pairing + relay-pairing + persist
work landed on branch `feat/mobile-companion`. Read [`phase1.md`](phase1.md) for
the overall design and the A (gateway) / C (remote-host) / P (phone) roles.

## What already landed (for context)

- **Pairing**: Bluetooth-style mutual-confirm handshake (both the phone user and
  the operator confirm a code derived from the SPAKE2 secret; no separate
  `device approve` step; device rows are `Approved` from creation). `device pair`
  is interactive, label is optional (the device reports its own name), renders a
  terminal QR, default endpoint `wss://proxy.baybo.space:7777`.
- **Relay pairing (C+A+P)**: `remote-host-relay` binary hosts the blind
  rendezvous (`/pair/host/{code}` admission-gated by `x-instance-key`,
  `/pair/join/{code}` for the app). The gateway's `channel/relay_pair.rs` host
  manager parks a leg per live slot; the app joins `/pair/join/<code>` when the
  QR carries `&relay=1`. `drive()` is transport-generic (`PairTransport`).
- **Persist**: after pairing the app stores a `PairedRecord` (auth token,
  gateway static key, routing candidates, relay_node_id, Noise static secret) in
  the App Group keychain; `paired_user` + a React "remembered" view on launch.

Everything above is committed + verified (cargo test/clippy on the root + iOS
workspaces, tsc on the frontend, 13 relay tests).

---

## TODO

### 1. Direct content session — a paired device can chat ✅ done

Gateway `/v1/device/content` Noise-IK responder (+ the transport-generic
`FrameSink`/`FrameSource` seam in `channel/adapter.rs`) + Tauri content wiring +
chat UI. See `crates/gateway/src/channel/device_content.rs`,
`app/mobile/ios/src-tauri/src/content.rs`, `app/mobile/ios/src/App.tsx`. (Known
limit: one frame ≤ the Noise ~64 KiB ceiling; chunking is a follow-up.)

### 2. Content relay — content traffic for a NAT'd gateway (phase 2)

Relay **pairing** works; relay **content** does not. Today `relay_node_id` is
`String::new()` (`crates/gateway/src/channel/device_pair.rs`), the relay only
serves the pairing routes, and the A↔C control connection
(`crates/gateway/src/relay/mod.rs`, `remote-host/crates/relay/src/control.rs`)
isn't wired into boot. To let content ride the relay:

- gateway opens + maintains its A↔C control connection at boot; C assigns a
  `relay_node_id`; the gateway puts it in `GatewayWelcome`.
- `remote-host-relay` serves a content-leg route keyed by `relay_node_id`
  (reuse `RelayBroker` + the `pump_ws` adapter, like the pairing routes).
- the app's content session falls back to the relay leg when no direct candidate
  connects (`connect_first` already plans `Endpoint::Relay`).

Depends on #1 (the content session must exist first).

### 3. Real APNs device token

`app/mobile/ios/src-tauri/src/pairing.rs` `TODO(apns)`: the app sends an **empty**
`apns_token` in `DeviceHello`. Wire it from
`didRegisterForRemoteNotifications` (the token arrives async after
`registerForRemoteNotifications` in `push_register.rs`) and thread it into the
pairing request (or a later `/register`).

### 4. External blockers (need resources, not just code)

- **M4 real end-to-end APNs delivery**: paid Apple Developer account + a real
  device (the simulator can't receive real APNs).
- **App Group keychain / provisioned, code-signed build**: paid account + Xcode
  automatic signing, so the NSE can read the push key on-device (see the
  empirical boundary notes in `phase1.md`). Until then the live NSE decrypt path
  is unverifiable.

### 5. Hardening / smaller follow-ups

- **Relay**: TTL sweep for stale parked host legs (a crashed gateway leaves a
  parked leg until the WS closes); basic rate-limiting on `/pair/join`.
- **relay-pair host manager**: no shutdown signal — it dies with the process;
  give it a `ShutdownSignal` if graceful drain matters.
- **No automated cross-workspace e2e** for relay pairing (relay and gateway are
  separate Cargo workspaces); the full path is only proven on deployment.
- **Multi-gateway persist**: the `PairedRecord` is stored under one fixed account
  (`baybo.paired-gateway`); pairing with a second gateway overwrites it. Key by
  device id + a "current" pointer if multi-gateway is needed.
- **PR #132** is closed; reopen or open a fresh PR when ready.

---

## Deploying what's built (relay pairing)

1. Deploy `remote-host-relay` behind the `proxy.baybo.space:7777` TLS terminator:
   `RELAY_INSTANCE_KEYS=<gateway instance_key>` (comma-separated), optional
   `RELAY_BIND_ADDR` (default `0.0.0.0:8444`).
2. Gateway `baybo.json`:
   `"relay": { "enabled": true, "url": "wss://proxy.baybo.space:7777", "instance_key": "<same key>" }`.
