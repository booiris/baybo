# Baybo Testing Conventions

This guide defines the test layout and reusable framework for the
workspace. Read this before adding new tests, especially when the work
crosses crate boundaries.

This document covers the **Rust** workspace. The `app/web` dashboard's
TypeScript/vitest suite has its own conventions — mostly pure-logic reducers,
plus a thin React Testing Library layer for the surfaces whose wiring a reducer
test cannot reach — documented in
[`web-unit-tests.md`](web-unit-tests.md).

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
| domain crates      | `MemoryTurnStore` (`baybo-turn`), `MemoryTraceStore` (`baybo-trace`), `MemoryCostStore` (`baybo-cost`), `RecordingMemory` (`baybo-memory` — records `recall` / `on_turn_complete` calls), `MemorySessionStore` + `MemorySessionFolderStore` (`baybo-session`) | In-memory backends for the `*Store` traits (the trait contracts live in `baybo-store`; each fake sits in its domain crate's `test_support.rs`). Each exposes a typed `Arc` handle so e2e tests can assert on what the agent persisted. `MemorySessionStore` stubs out lineage lookups (`list_lineage_children` returns empty); tests that need that surface should use the real sqlite store via `Store::open` against a tempfile. |
| `baybo-tools`       | `EchoTool`, `RecordingTool`                                             | `Tool` impls — `EchoTool` echoes params; `RecordingTool` captures invocation params.          |
| `baybo-llm`         | `StubLlm`                                                               | Scriptable `LlmCompletion` impl. `with_text_chunk_size(n)` forces sub-chunked stream events. |
| `remote-host-protocol` | `AddressPolicy::for_tests`                                          | With `test-support`, `AddressPolicy::active()` returns it: 127/8, 198.18/15 and 2001:2::/48 are `Public` and `::1` is `Lan`, so direct-carrier tests run over loopback. `remote-host-relay`'s and `baybo-gateway`'s tests enable it. Features unify per build, so any crate tested in the same `cargo`/`nextest` invocation as those sees the test policy too: tests that classify addresses use data that reads the same under either policy. |
| `remote-host-relay` | `ControlRegistry::with_direct_answer_timeout`, `ConnectionRegistry::register_for_test` | Shortens how long `POST /direct` waits for the gateway's report, so the no-answer path does not sleep 3 s; registers a live connection to drive a revoke kick. |
| `carrier`          | `DemuxSocket::inject_recv_errors`                                       | Records failed receives on a real socket, backing off as real ones do, so the gateway's rebind test drives the receive-error streak its supervisor polls. |
| `baybo-gateway`    | `examples/seed_relay_binding.rs` (and the forwarded `remote-host-protocol/test-support`) | Writes an approved relay binding into a workspace and prints the phone's pairing record — the netns matrix's gateway setup. A `baybo` built with `--features baybo-gateway/test-support` uses the test address policy. |
| `baybo-ios-ffi` (`app/ios`) | in-memory keychain, `test_support::{seed_relay_pairing, hold_chat_rotation}`, `examples/netns_phone.rs` | A host keychain that keeps writes, so the netns matrix's phone process is seeded with a pairing; a knob that holds chat rotation off; the protocol's test address policy. App builds never enable it, and `ios-core`'s nextest runs without it. |
| `baybo-workspace`   | `back_date`, `back_date_tree`, `back_date_symlink`                      | mtime back-dating for tests that drive the `walk::tree_stats` staleness gates (janitor sweeps). `back_date_symlink` sets the link's *own* lstat mtime via `utimensat(AT_SYMLINK_NOFOLLOW)`. |

The integration-tests crate composes these into higher-level builders:

| Helper                          | Where                                              | Purpose                                                  |
| ------------------------------- | -------------------------------------------------- | -------------------------------------------------------- |
| `gateway_with_memory_vault()`   | `baybo_integration_tests::fixtures`                 | Returns `(Arc<SecurityGateway>, Arc<MemorySecretStore>, Arc<SecretVault>)` — the full security pipeline wired against an in-memory vault. |
| `SessionBuilder`                | `baybo_integration_tests::fixtures`                 | Fluent builder for `Session` so tests don't repeat field lists. |
| `master_key_for_tests()`        | `baybo_integration_tests::fixtures`                 | Stable 32-byte `EncryptionKey` so placeholder hex stays reproducible across runs. |
| `capture_tracing()`             | `baybo_integration_tests::tracing_capture`          | Per-test thread-local `tracing` subscriber. Returns `TracingCapture` (RAII) with `events()`, `at_level(Level)`, `any_contains(&str)`. |
| `AgentTestHarnessBuilder` / `AgentTestHarness` | `baybo_integration_tests::harness`   | Spawns a real `AgentActor` wired to the in-memory stores, the `StubLlm`, and the gateway. Tests push canned LLM responses, send user input via `harness.send_text(...)` (which runs `SecurityGateway::sanitize_input` first, just like the real `Router`), then drain `AgentOutput` from the channel side. `with_tool(Arc<dyn Tool>, ToolManifest)` registers tools before the actor spawns. `send_content(Vec<ContentBlock>)` sends arbitrary user content (an uncaptioned image). The conversation-title pass is off unless `with_title_sink(..)` wires a sink — its `chat()` then draws on the same stub, so queue a `push_response` for it — and `with_model_vision(true)` makes the stub report `supports_vision`. |

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
  summariser retry, and the failure contract (transcript kept whole,
  step closed `Failed`, one `Warn` notice to the user).
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

  A probe that dies mid-scenario fails the test. The harness surfaces that
  as a distinct `HarnessError::ProcessDied` (rather than burning the whole
  timeout on a dead pane), and the chat TUI issues no terminal queries at
  all — the inline viewport anchors from bookkeeping, so `EventStream` is
  the only stdin reader and there is no race left to tolerate. A death here
  means the event loop took an error path, which is a real regression.

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

The direct-carrier tests that run over IPv6 loopback self-skip, through
`carrier::phone::ipv6_loopback_available`, on a host that cannot bind `::1`
(a container with IPv6 disabled). The carrier's IPv6 interface flags have an
Apple-only reader; CI's `gateway-macos` job runs its tests on macOS whenever
`crates/carrier/` or `crates/gateway/src/channel/carrier/` changes.

### Direct-carrier NAT matrix

This Linux integration test runs the real relay, gateway and iOS Rust networking
core under simulated router/NAT conditions. It verifies carrier selection,
relay fallback, idle keepalives and chat rotation. It does not run the iPhone UI
or validate iOS VPN routing, Local Network permission, real relay HTTPS, or
NAT64 without CLAT. The topology and expected outcomes live in
[the carrier spec](modules/mobile/direct-carriers.md#testing); hardware checks
live in [the iOS device checklist](../app/ios/docs/testing.md#manual-verification-checklist-device).

Use a disposable Linux VM or CI runner. The test is ignored by ordinary test
runs and is not part of the production app or gateway. It requires root,
`ip`, `iptables`, `ip6tables`, `tc`, `sqlite3`, and `sch_netem` support for the
packet-loss case. It cannot run natively on macOS.

```bash
scripts/netns-matrix.sh                                   # every cell, 5 runs each
NETNS_CELLS="cone/cone same-lan" NETNS_RUNS=1 scripts/netns-matrix.sh
```

`NETNS_IDLE_SECS` defaults to 40 seconds; shortening it weakens the idle
keepalive check. The script builds all three workspaces with `test-support`,
exports `NETNS_*_BIN` paths and invokes the ignored test under `sudo`.
Do not use these test-policy binaries as your normal gateway installation.
The non-gating `netns-matrix` CI job runs only on non-draft PRs matching its
path filters; a skipped draft job is not evidence the matrix passed.

#### Isolation and cleanup

- Bridges, virtual links, routes, NAT/firewall rules, forwarding settings and
  packet loss are configured inside the test namespaces. No test link is
  attached to the host's physical interfaces; host routes, DNS and VPN settings
  are not rewritten. Namespaces share the host kernel and consume host resources;
  this is not a VM security boundary.
- Namespace names are currently `bnm<run_index>-<role>`, with roles `a`,
  `nata`, `inet`, `natp`, `p` and `c`. Setup deletes existing namespaces
  with those exact names. **Run only one matrix per host**, and do not use that
  naming scheme for unrelated namespaces. Concurrent checkouts also collide.
- Normal completion and Rust panic unwinding attempt to kill/wait for child
  processes and delete the namespaces. Cleanup errors are ignored. Signals
  that terminate the runner without unwinding, including SIGKILL, can leave
  processes and namespaces behind. Reboot removes live network state, but
  test files may remain.
- Workspaces, databases and logs remain under
  `/tmp/netns-matrix-<pid>-<cell>-<run_index>`. Running Cargo through `sudo`
  can also leave root-owned build files in `app/ios/target`.

After an interrupted run, inspect `sudo ip netns list` and
`sudo ip netns pids <exact-name>`. Confirm no matrix is still running and
identify the resources belonging to that run before stopping its processes
and deleting its namespaces. Deleting a namespace name alone does not stop
processes that still hold it. Avoid a broad namespace deletion or host firewall
flush. In a disposable VM, discarding the VM is the simplest complete cleanup.

### Remote-host workspace

`remote-host/` is its own cargo workspace, excluded from the root one, so the
commands above never run its tests. Run them from inside it
(`cd remote-host && cargo nextest run --workspace`). nextest reads that
workspace's own `remote-host/.config/nextest.toml`, which carries the same
hang guard as the root config (`slow-timeout` terminating after 4 × 30 s). In
CI, the `remote-host` job runs fmt, clippy and nextest there on every PR that
touches `remote-host/` or `rust-toolchain.toml`.
