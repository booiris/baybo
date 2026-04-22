# channel-ts SDK — drift prevention

Rust (`crates/channels/src/wire.rs`) and TS (`sdks/channel-ts/src/`)
implement the same wire protocol. Wire types and `PROTOCOL_VERSION`
are single-sourced from Rust via `ts-rs` (see `ts-export` feature in
`crates/channels/Cargo.toml`). The TS surface consumed by sidecars is
the `Channel` interface + `runChannel()` in `src/channel.ts` /
`src/runner.ts`; wire types are re-exported under the `./wire`
subpath for advanced use.

- [x] **CI gate on regenerated bindings.** `scripts/check-ts-bindings.sh`
  runs `cargo test -p aura-channels --features ts-export --lib wire`
  and diffs `sdks/channel-ts/src/generated/` against HEAD with
  `git diff --exit-code`. Wire into whatever CI lands; locally, run it
  from the repo root before committing a wire-type change.

- [x] **End-to-end round-trip test.** `sdks/channel-ts/test/e2e-roundtrip.test.mjs`
  stands up a `ws`-based fixture server, drives a stub `Channel` via
  `runChannel`, and asserts Register/Ack, Delta/Notice/Message,
  ApprovalRequested → ResolveApproval (`approve_always`),
  ApprovalResolved, and SidecarLog all round-trip cleanly. Also
  covers the `register_ack=false` fatal path.

- [x] **Approval `approve_always` pass-through test.** Covered by the
  unit test in `sdks/channel-ts/test/approval.test.mjs`
  (`normalizeDecision` accepts all three variants) and the e2e test
  (`approve_always` survives the server → handler → wire → server
  round-trip).

- [x] **Forward sidecar logs to aura over the wire.** `Frame::SidecarLog
  { level, text, target? }` is the sidecar→server carrier (`wire.rs`,
  additive — `PROTOCOL_VERSION` unchanged). The gateway WS route
  parses the level, truncates at 1 KB, and pushes into
  `LogBuffer::push_external` with `target = "sidecar::<channel_type>"`
  (or `"sidecar::<channel_type>::<sidecar-target>"` when the sidecar
  supplies one). On the SDK side `defaultLogger` still writes to
  stdout and, while the WS is open, forwards each line via the sink
  installed by `runChannel`; custom loggers (pino / winston) don't
  implement `setWireSink` so they stay local. Rate cap:
  100 lines/1000ms per connection; overflow is dropped with a single
  summary warn line. Crash logs before the first handshake and during
  reconnect backoff still go to stdout only.
