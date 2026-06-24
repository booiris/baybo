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

- **Relay**: TTL sweep for stale parked broker legs — both the pairing
  `/pair/host` legs **and** the content `/content/join` / `/content/host` legs
  leak a parked entry if the partner never arrives or the WS upgrade fails after
  the synchronous `broker.join` (a crashed/slow gateway, a client that drops
  mid-handshake). A periodic sweep keyed on park-time is the systematic fix.
  Also basic rate-limiting on `/pair/join` and `/content/join` (both park a leg
  for an unauthenticated caller that knows only a code / `relay_node_id`).
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
