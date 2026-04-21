# Aura Testing Conventions

This guide defines the test layout and reusable framework for the
workspace. Read this before adding new tests, especially when the work
crosses crate boundaries.

## Three-layer pyramid

| Layer        | Where                                            | What it covers                                                                              |
| ------------ | ------------------------------------------------ | ------------------------------------------------------------------------------------------- |
| Unit         | `crates/<crate>/src/**/*.rs` `#[cfg(test)] mod`  | Single function / struct logic. No I/O, no async unless the unit is async.                  |
| Crate-level  | `crates/<crate>/tests/*.rs`                      | Public API of one crate, run as a separate binary. May exercise its own `test-support`.     |
| Cross-crate  | `crates/integration-tests/tests/*.rs`            | End-to-end scenarios that wire multiple crates' `test-support` features together.           |

The pyramid is wide at the base. A new feature should pick up unit
coverage first, lift its public surface into a crate-level test once
that surface stabilizes, and finally land an e2e test only for the
contracts that span crates (e.g. the security boundary, streaming
pipeline).

## Test-support gating

Test helpers consumed across crates live behind a `test-support` cargo
feature so they never ship in release builds. The pattern:

```toml
# Producer crate's Cargo.toml
[features]
test-support = []
```

```rust
// Producer crate's lib.rs
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
```

```toml
# Consumer crate's Cargo.toml
[dev-dependencies]
aura-foo = { workspace = true, features = ["test-support"] }
```

Helpers used only by the same crate's tests stay `#[cfg(test)]`.

## Available test-support fixtures

| Crate              | Helper                                                                  | Purpose                                                                                       |
| ------------------ | ----------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `aura-storage`     | `MemorySecretStore`                                                     | In-memory `SecretStore` impl with `len()` / `is_empty()` for vault-state assertions.          |
| `aura-storage`     | `MemoryJobStore`, `MemoryCostStore`, `MemoryTraceStore`, `MemoryMemoryStore` | In-memory backends for the remaining `Store` traits. Each exposes a typed `Arc` handle so e2e tests can assert on what the agent persisted. |
| `aura-tools`       | `EchoTool`, `RecordingTool`                                             | `Tool` impls — `EchoTool` echoes params; `RecordingTool` captures invocation params.          |
| `aura-llm`         | `StubLlm`                                                               | Scriptable `LlmCompletion` impl. `with_text_chunk_size(n)` forces sub-chunked stream events. |

The integration-tests crate composes these into higher-level builders:

| Helper                          | Where                                              | Purpose                                                  |
| ------------------------------- | -------------------------------------------------- | -------------------------------------------------------- |
| `gateway_with_memory_vault()`   | `aura_integration_tests::fixtures`                 | Returns `(Arc<SecurityGateway>, Arc<MemorySecretStore>, Arc<SecretVault>)` — the full security pipeline wired against an in-memory vault. |
| `SessionBuilder`                | `aura_integration_tests::fixtures`                 | Fluent builder for `Session` so tests don't repeat field lists. |
| `master_key_for_tests()`        | `aura_integration_tests::fixtures`                 | Stable 32-byte `EncryptionKey` so placeholder hex stays reproducible across runs. |
| `capture_tracing()`             | `aura_integration_tests::tracing_capture`          | Per-test thread-local `tracing` subscriber. Returns `TracingCapture` (RAII) with `events()`, `at_level(Level)`, `any_contains(&str)`. |
| `AgentTestHarnessBuilder` / `AgentTestHarness` | `aura_integration_tests::harness`   | Spawns a real `AgentActor` wired to the in-memory stores, the `StubLlm`, and the gateway. Tests push canned LLM responses, send user input via `harness.send_text(...)` (which runs `SecurityGateway::sanitize_input` first, just like the real `Router`), then drain `AgentOutput` from the channel side. `with_tool(Arc<dyn Tool>, ToolManifest)` registers tools before the actor spawns. |

## Six conventions

1. **Fixture colocation.** Fakes, builders, and stubs live next to the
   crate that owns the abstraction, gated by `test-support`. Don't
   duplicate them in consumer crates.

2. **Builder pattern for domain types.** Tests construct `Session`,
   `Message`, `OutgoingMessage` etc. via builders so a future field
   addition doesn't fan out across every test file. `SessionBuilder`
   in `aura-integration-tests` is the reference pattern.

3. **Spy over mock.** Prefer recording fakes (e.g. `RecordingTool`)
   that capture all interactions and let the test assert on the actual
   call history. Avoid expectation-style mocks — they couple tests to
   call ordering.

4. **Per-test tracing capture.** Use `capture_tracing()` rather than
   the global subscriber so tests don't trample each other and don't
   depend on init order. The capture installs via
   `tracing::subscriber::set_default`, which is thread-local; the
   guard restores the prior subscriber on drop.

5. **Three-layer pyramid.** Match the test layer to the contract under
   test. A function's logic belongs in a unit test, a crate's public
   API in `crates/<crate>/tests/`, and a contract that spans crates
   (security boundary, streaming pipeline) in
   `crates/integration-tests/tests/`.

6. **Per-crate `test-support` feature.** Cross-crate test helpers are
   gated behind an opt-in cargo feature. Same-crate helpers stay
   `#[cfg(test)]`. Never leave a test-only `pub` helper ungated.

## Spec-drift tests

Snapshot files committed to the repo are kept honest by a dedicated
crate-level test that regenerates the file from the source of truth and
compares byte-for-byte. The convention:

- Test sets an `UPDATE_<THING>=1` env var escape hatch that rewrites the
  snapshot instead of asserting.
- Failure prints the exact command a developer should run to regenerate.
- The regenerated file is checked in, so CI (which does not set the env
  var) fails whenever the snapshot and the code disagree.

Current drift tests:

| Test                                            | Snapshot                | Regenerate with                                                            |
| ----------------------------------------------- | ----------------------- | -------------------------------------------------------------------------- |
| `crates/gateway/tests/openapi_spec_sync.rs`     | `docs/openapi.json`     | `UPDATE_OPENAPI=1 cargo test -p aura-gateway --test openapi_spec_sync`     |

The OpenAPI snapshot is the contract that `web/`'s
`openapi-typescript` codegen reads to produce `web/src/api/schema.d.ts`;
keeping it in lockstep with the Rust router is what lets the frontend
`tsc` step catch API drift.

## End-to-end suite layout

`crates/integration-tests/tests/` currently hosts:

- `smoke.rs` — fixture wiring sanity check.
- `security_pipeline.rs` — input → mint → vault → reveal → output, plus
  block-rule, audit, and injection-log assertions.
- `streaming_safety.rs` — placeholder integrity across stream deltas
  and the high-water flush invariant.
- `tool_boundary.rs` — reveal-on-call, sanitize-on-return, tool-output
  envelope and forged-close-tag neutralization.
- `agent_loop_e2e.rs` — drives the full `IncomingMessage → gateway →
  AgentActor → AgentLoop → StubLlm → AgentOutput` path through
  `AgentTestHarness`. Pins clean-stream deltas, secret minting at the
  router seam, the tool-call round trip, and inbound injection
  warnings.

Each file pins one cross-cutting contract. New e2e tests should follow
the pattern: name the file after the contract, group scenarios as
`#[tokio::test]` functions whose names read as the assertion.

## Running tests

```bash
cargo test                                              # full workspace
cargo test -p aura-security                             # one crate
cargo test -p aura-integration-tests --test security_pipeline   # one file
cargo test -p aura-integration-tests --test tool_boundary -- --nocapture
```

CI runs `cargo clippy --all --benches --tests --examples --all-features`
with zero-warnings; new tests must clear that gate.
