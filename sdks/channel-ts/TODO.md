# channel-ts SDK — drift prevention

Rust (`crates/channels/src/sdk/`) and TS (`sdks/channel-ts/src/`)
implement the same wire protocol. Types and `PROTOCOL_VERSION` are
already single-sourced from Rust via `ts-rs` (see `ts-export` feature
in `crates/channels/Cargo.toml`). Remaining drift gaps:

- [ ] **CI gate on regenerated bindings.** Run
  `cargo test -p aura-channels --features ts-export` then
  `git diff --exit-code sdks/channel-ts/src/generated/`. Fails the job
  if a wire-type change landed without regeneration.

- [ ] **End-to-end round-trip test.** Stand up a Rust fixture WS server
  (accept one connection, verify `Register`, echo one `Message`), and
  a `node --test` / `vitest` suite that drives the TS `Client` against
  it. This is the only way MessagePack tag rename, `rename_all` flip,
  or serde attribute tweaks surface automatically — type generation
  alone won't catch encoding changes. Blocked on the gateway growing a
  real WS listener (same server code can double as the fixture).
