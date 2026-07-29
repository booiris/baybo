# Baybo Testing Conventions

This guide defines the test layout and reusable framework for the
workspace. Read this before adding new tests, especially when the work
crosses crate boundaries.

This document covers the **Rust** workspace. The `app/web` dashboard's
TypeScript/vitest suite has its own conventions — mostly pure-logic reducers,
plus a thin React Testing Library layer for the surfaces whose wiring a reducer
test cannot reach — documented in
[`todo/web-unit-tests.md`](todo/web-unit-tests.md).

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
baybo-foo = { workspace = true, features = ["test-support"] }
```

Helpers used only by the same crate's tests stay `#[cfg(test)]`.

## Available test-support fixtures

| Crate              | Helper                                                                  | Purpose                                                                                       |
| ------------------ | ----------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `baybo-security`    | `MemorySecretStore`                                                     | In-memory `SecretStore` impl with `len()` / `is_empty()` for vault-state assertions.          |
| domain crates      | `MemoryTurnStore` (`baybo-turn`), `MemoryTraceStore` (`baybo-trace`), `MemoryCostStore` (`baybo-cost`), `RecordingMemory` (`baybo-memory` — records `recall` / `on_turn_complete` / `on_session_end` calls), `MemorySessionStore` + `MemorySessionSummaryStore` (`baybo-session`) | In-memory backends for the `*Store` traits (the trait contracts live in `baybo-store`; each fake sits in its domain crate's `test_support.rs`). Each exposes a typed `Arc` handle so e2e tests can assert on what the agent persisted. `MemorySessionStore` stubs out lineage lookups (`list_lineage_children` returns empty); tests that need that surface should use the real sqlite store via `Store::open` against a tempfile. `MemorySessionSummaryStore` mirrors the sqlite backend's per-row semantics (`upsert_success` advances `cursor` monotonically and resets `error_count`, `bump_error_count` inserts a zero row when missing and accumulates the failed pass's spend) so unit tests assert against the same invariants production exercises. |
| `baybo-tools`       | `EchoTool`, `RecordingTool`                                             | `Tool` impls — `EchoTool` echoes params; `RecordingTool` captures invocation params.          |
| `baybo-llm`         | `StubLlm`                                                               | Scriptable `LlmCompletion` impl. `with_text_chunk_size(n)` forces sub-chunked stream events. |
| `baybo-workspace`   | `back_date`, `back_date_tree`, `back_date_symlink`                      | mtime back-dating for tests that drive the `walk::tree_stats` staleness gates (janitor sweeps). `back_date_symlink` sets the link's *own* lstat mtime via `utimensat(AT_SYMLINK_NOFOLLOW)`. |

The integration-tests crate composes these into higher-level builders:

| Helper                          | Where                                              | Purpose                                                  |
| ------------------------------- | -------------------------------------------------- | -------------------------------------------------------- |
| `gateway_with_memory_vault()`   | `baybo_integration_tests::fixtures`                 | Returns `(Arc<SecurityGateway>, Arc<MemorySecretStore>, Arc<SecretVault>)` — the full security pipeline wired against an in-memory vault. |
| `SessionBuilder`                | `baybo_integration_tests::fixtures`                 | Fluent builder for `Session` so tests don't repeat field lists. |
| `master_key_for_tests()`        | `baybo_integration_tests::fixtures`                 | Stable 32-byte `EncryptionKey` so placeholder hex stays reproducible across runs. |
| `capture_tracing()`             | `baybo_integration_tests::tracing_capture`          | Per-test thread-local `tracing` subscriber. Returns `TracingCapture` (RAII) with `events()`, `at_level(Level)`, `any_contains(&str)`. |
| `AgentTestHarnessBuilder` / `AgentTestHarness` | `baybo_integration_tests::harness`   | Spawns a real `AgentActor` wired to the in-memory stores, the `StubLlm`, and the gateway. Tests push canned LLM responses, send user input via `harness.send_text(...)` (which runs `SecurityGateway::sanitize_input` first, just like the real `Router`), then drain `AgentOutput` from the channel side. `with_tool(Arc<dyn Tool>, ToolManifest)` registers tools before the actor spawns. |

## Six conventions

1. **Fixture colocation.** Fakes, builders, and stubs live next to the
   crate that owns the abstraction, gated by `test-support`. Don't
   duplicate them in consumer crates.

2. **Builder pattern for domain types.** Tests construct `Session`,
   `Message`, `OutgoingMessage` etc. via builders so a future field
   addition doesn't fan out across every test file. `SessionBuilder`
   in `baybo-integration-tests` is the reference pattern.

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
| `crates/gateway/tests/openapi_spec_sync.rs`     | `docs/openapi.json`     | `UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync` |

The OpenAPI snapshot is the contract that `app/web`'s
`openapi-typescript` codegen (`pnpm gen:api`) reads to produce `app/web/src/api/schema.d.ts`;
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
- `channel_registration.rs` — drives the real Telegram sidecar bundle
  through the production registration driver.
- `context_compression_e2e.rs` — the blocking context-compaction path
  under a tight token budget: cost/span join, the status pair, the
  summariser retry, and the spanless truncate step.
- `token_calibration_e2e.rs` — the token-count calibration feedback loop.
- `tool_concurrency.rs` — tool-call concurrency scheduling in
  `run_iteration`.

(`all.rs` is the aggregator, not a suite.)

Each file pins one cross-cutting contract. New e2e tests should follow
the pattern: name the file after the contract, group scenarios as
`#[tokio::test]` functions whose names read as the assertion. The crate
sets `autotests = false` and links every e2e file into one `all` test
binary — a new file must also be mounted in `tests/all.rs`
(`#[path = "my_contract.rs"] mod my_contract;`) or it silently never
builds or runs. The same aggregator convention applies to the
crate-level suites in `crates/{security,memory,sandbox,cli,gateway,config}`.

## Real-terminal rendering tests

Terminal rendering — raw crossterm escape sequences, the alternate
screen, inline-viewport anchoring, and the SIGWINCH/resize reflow path —
can't be checked by unit tests over the layout math. The
`baybo-term-harness` crate drives the **actual binary** inside a detached
tmux pane at a forced size and reads back the rendered screen with
`capture-pane`. tmux interprets escape sequences exactly like a real
terminal, so the capture is ground truth (a raw PTY would hand the bytes
back uninterpreted and hide the bugs); this is the same technique that
caught the TUI's inline-viewport resize ghosting.

The harness API: `TmuxSession::launch(LaunchSpec { program, args, width,
height, env })`, then `send_keys`/`send_text`, `resize`, `capture`, and
the settle-aware `wait_until` / `wait_stable` / `wait_for_exit` pollers
(no fixed sleeps). `tmux_available()` lets a test self-skip when tmux is
absent, so CI without tmux stays green — the same self-skip contract as
the docker/bwrap-backed tests. The suites are also `#[ignore]` (they're
flaky under load), so the gating `test` job never runs them: tmux is
installed only in the separate non-gating `render-tests` job, which is
`continue-on-error` and fires only when a PR touches `crates/tui` or
`crates/term-harness`, via `-- --include-ignored`.

The probe pattern (used by both suites below):

- The thing under test is launched as a small **probe binary** living in
  the crate under test (`src/bin/<probe>.rs`), gated with
  `required-features = ["test-support"]` so it never builds or ships in a
  release build. The crate enables that feature during its own tests via
  the dev-dependency self-reference
  (`baybo-foo = { workspace = true, features = ["test-support"] }`), which
  is what makes cargo build the bin and expose its path to the test as
  `env!("CARGO_BIN_EXE_<probe>")`. Locating the binary this way avoids a
  nested `cargo build` at test time (which contends on cargo's target
  lock and is pathologically slow).
- A probe that finishes its work and exits would lose its final frame:
  tmux's `remain-on-exit` keeps the pane but scrolls a row off and
  overlays a "Pane is dead" footer. So probes **block after their work**
  (the chat probe runs until Ctrl+C) and the test captures while the
  program is still alive. The harness kills the pane on `Drop`.

Current real-terminal suites:

- `crates/tui/tests/chat_render.rs` (probe `chat_smoke`) — the
  inline-viewport chat UI driven against an in-process stub gateway that
  speaks `baybo_channels::wire`. The stub dispatches on the typed message
  (`baybo_tui::smoke_contract`) so one probe covers many scenarios:
  - **Golden snapshots** for the clean, stable frames — the initial
    banner and a plain reply — stored under `tests/snapshots/*.snap` and
    compared after `normalize()` masks the version string (`vX.Y.Z`) and
    drops the volatile working-indicator timer line. Regenerate after an
    intentional UI change with `UPDATE_CHAT_SNAPSHOT=1 cargo test -p
    baybo-tui --test chat_render -- --include-ignored`. These catch *unanticipated* visual
    drift the structural asserts would miss.
  - **Structural assertions** for the dynamic scenarios: a tool-call line
    (`Read(src/lib.rs)` + `⎿` result), a subagent surfacing as a `Task`
    tool call, the tool-approval modal (`wants to run` … `[a] Approve` /
    `[d] Deny`) and its resolution, and the contract that `Frame::TaskList`
    is **dropped** by the TUI (the planning checklist is web-dashboard
    only, so the task subject must *not* appear). The post-resize frame
    stays structural too — the inline-viewport resize has a known,
    accepted cosmetic ghost frame, so a golden there would be flaky.

  Driving the chat UI exposes a known, accepted race: a non-keyboard
  viewport rebuild queries cursor position (`ESC[6n`), and because
  dropping crossterm's `EventStream` doesn't synchronously stop its reader
  thread, a lingering `stdin` read can steal the reply, time out, and exit
  the process mid-turn. That's orthogonal to rendering correctness, so the
  harness returns a distinct `HarnessError::ProcessDied` (rather than
  burning the whole timeout on a dead pane) and the test retries the
  scenario on a fresh process; a genuine render mismatch (process alive,
  output wrong) still fails fast.

## Running tests

```bash
cargo nextest run --workspace                           # canonical runner (CI's gating job; config in .config/nextest.toml)
cargo test                                              # full workspace (fallback)
cargo test -p baybo-security                             # one crate
cargo test -p baybo-integration-tests --test all security_pipeline::   # one file (module filter)
cargo test -p baybo-integration-tests --test all tool_boundary:: -- --nocapture
cargo test -p baybo-tui   --test chat_render -- --include-ignored   # real-terminal (needs tmux; suites are #[ignore])
```

The real-terminal suites need `tmux` on `PATH`; without it they self-skip
(pass with a skip note) rather than fail.

CI runs `cargo clippy --all --benches --tests --examples --all-features`
with zero-warnings; new tests must clear that gate.
