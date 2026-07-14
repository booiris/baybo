# Baybo Development Guide

**Baybo** is an intelligent assistant framework built on large language models, supporting multi-channel access, tool invocation, skill extensions, with comprehensive context management, compression, and error recovery mechanisms.

## Session data is core data — never delete

Session rows and their conversation transcripts are **user-facing core data**. They must never be deleted by the runtime:

- No `DELETE FROM sessions` in production paths. No "expire and drop" sweep. No "idle cleanup" that touches the row.
- The `hidden` flag is the user-facing "remove from my list" affordance — the row, transcript, summary cursor, and channel binding stay live so the user can recover the conversation.
- The actor reaper (`AgentSupervisor::reap_idle` / `spawn_idle_reaper`) is allowed to shut down the in-memory `AgentActor` for an idle session. The session row stays in the store; hydration on the next user message rebuilds the actor from durable state. The reaper must only operate on actors, never on rows.
- Do not add a `cleanup_expired`-style API. If a future feature needs row-level retention (user-requested wipe, GDPR delete-on-request), gate it explicitly behind a user-triggered command, not a background sweeper.

## Build & Test

```bash
cargo fmt                                                       # format
cargo clippy --all --benches --tests --examples --all-features  # lint (zero warnings)
cargo nextest run --workspace                                   # unit tests (fast runner — CI uses this)
cargo test --workspace                                          # unit tests (no nextest; also runs doctests, which are disabled)
RUST_LOG=baybo=debug cargo run                                   # run with logging

pnpm install                                                    # hydrate TS workspaces
pnpm --filter @baybo/channel-sdk test                            # SDK unit + e2e tests
scripts/check-ts-bindings.sh                                    # ts-rs regen CI gate
```

**Test runner.** `cargo nextest run --workspace` is the canonical runner: it runs every test as its own process in one shared core pool, instead of cargo's serial-per-binary execution. Install once with `cargo install cargo-nextest` (or `curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C ~/.cargo/bin`). Config lives in `.config/nextest.toml` (a `tmux-serial` test-group serializes the tmux render tests). Plain `cargo test --workspace` still works. Do **not** add `--all-features` to the test run: it enables `baybo-tools/bench-bash`, which flips the bash tools into bench mode and fails the non-bench bash assertions; the clippy gate uses `--all-features` (compile-only) but the test gate must not.

**Slow tests are `#[ignore]` or gated.** The tmux render/smoke tests (`crates/tui/tests/chat_render.rs`, the `baybo-term-harness` lib tmux tests) are `#[ignore]` — they're flaky under load and run in CI's non-gating `render-tests` job via `--include-ignored`. Docker- and bwrap-backed sandbox smokes self-skip when their backend is absent. Any test that must sleep on a real timeout (e.g. the OpenViking timeout paths) injects ms-scale budgets — see `OpenVikingMemory::with_timeouts` — rather than sleeping real seconds.

**Python tooling uses `uv` with a persistent project venv — never bare `pip` or the system interpreter.** Project-side Python (currently the `bench/` harnesses — `bench/swe`, `bench/swe-baseline`, `bench/swe-claude`, and the two `bench/terminal-bench-*` dirs) declares its deps in a `pyproject.toml`; `uv sync` materialises a reused on-disk `.venv` **and** provisions a pinned CPython, so the env is built once, reproducible, and independent of whatever `python3` the host ships (the system one is often too new for a given stack — e.g. `swebench`). Run through it — `uv run --project <dir> python …`, or point a tool at `<dir>/.venv/bin/python`. Commit `pyproject.toml` / `uv.lock` / `.python-version`; gitignore `.venv/`. Add a new Python tool the same way (its own `pyproject.toml` + `uv sync`), not with a global `pip install`.

**Zero warnings means zero — including test files.** `--tests` is part of the clippy invocation above on purpose. Don't dismiss a warning as "pre-existing" or "only in a test"; if `cargo clippy` lights it up, fix it as part of the change.

**Pre-commit hook (recommended).** A tracked `.githooks/pre-commit` runs `cargo fmt --all --check` on any commit that stages Rust, so unformatted code is caught locally instead of by the `rustfmt` CI job. Enable it once per clone: `git config core.hooksPath .githooks`. Bypass a one-off with `git commit --no-verify`.

Test layout, `test-support` feature gating, and the shared fixture inventory are in [`docs/testing.md`](docs/testing.md). Read it before adding tests that cross crate boundaries.

Verbose / scoped logging:

```bash
RUST_LOG=baybo=trace cargo run                # verbose
RUST_LOG=baybo_agent=debug cargo run          # agent crate only
```

## Pull Requests

**Open every PR as a draft (`gh pr create --draft`), and never mark it ready
yourself.** The owner reviews it first and says when it may go ready. Marking it
ready is what starts CI and what puts it in front of reviewers — that is the
owner's call, not the author's.

```bash
gh pr create --draft --base master --head <branch> --title "…" --body "…"
# … owner reviews, and only then:
gh pr ready <number>
```

**A draft PR runs NO CI, and a skipped run looks exactly like a passing one.**
Every job in `.github/workflows/ci.yml` carries
`if: ${{ github.event.pull_request.draft == false }}` — deliberately, because a
skipped job spins up no runner and bills no minutes. The trap is on the reading
side: `gh pr checks --watch` on a draft reports every job as `skipping` and
**exits 0**, which is indistinguishable from green at a glance. Before merging,
confirm the gating jobs actually say `pass`. If they say `skipping`, CI has never
seen the code.

The macOS jobs cost 10× Linux minutes, which is why the iOS jobs are currently
`if: false` (see `61eb9246`). While that holds, **nothing under `app/ios` is
covered by CI** — the Rust core, the transcript bundle, and the Swift suite are
all local-only. Run them by hand and say so in the PR body; do not let a green
check imply coverage it does not have.

## Code Style

- Prefer `crate::` for cross-module imports; `super::` is fine in tests and intra-module refs
- No `pub use` re-exports unless exposing to downstream consumers
- No `.unwrap()` or `.expect()` in production code (tests are fine)
- Use `thiserror` for error types in `error.rs`; map errors with context: `.map_err(|e| SomeError::Variant { reason: e.to_string() })?`
- Prefer strong types over strings (enums, newtypes); use typed structs instead of `HashMap<String, Value>`, only keep an `extra` field for truly dynamic extensions
- No magic numbers / strings for values that have a single source of truth or cross a module boundary. Lift them into a named `const` (e.g. `SKILL_TOOL_NAME`, `MAX_SKILL_DIR_FILES`) and reference the const at every site. Two sites with the same literal are already a smell; three is a bug waiting to happen. Throwaway test fixtures and one-off log strings are fine inline.
- Keep functions focused, extract helpers when logic is reused
- Default to zero comments. Only add one when the WHY is non-obvious (hidden constraint, subtle invariant, workaround). Don't narrate WHAT the code does — well-named identifiers cover that.
- Avoid exporting unnecessary item, prefer `pub(crate)`; use `pub` only when necessary
- Test-only helpers (fakes, `NeverShutdown`-style stubs, dummy fixtures) MUST be gated so they don't ship in release builds:
  - Same-crate tests only → `#[cfg(test)]`.
  - Consumed by another crate's tests → gate with `#[cfg(any(test, feature = "test-support"))]` and add a `test-support = []` feature in `Cargo.toml`. Downstream crates pull them in via `baybo-<crate> = { workspace = true, features = ["test-support"] }` in `[dev-dependencies]`.
  - Never leave a test-only item plain `pub` — "it's named `Never...`" is not a gate.
- Required dependencies belong in the constructor, NOT behind a `with_*` setter. If a struct ends up with many required fields, define a sibling `XxxConfig` struct with `pub` fields and a single `pub fn from_config(config: XxxConfig) -> Self` constructor — callers populate it via struct literal so every required field shows up at the call site by name. `with_*` is reserved for: (a) **genuine config knobs** with a real default that some callers rationally leave alone (`with_rate_limit`, `with_timeout`); (b) **incremental builders** that append to a collection (`with_tool`, `add_rule`); (c) **truly optional deps** where some real production paths legitimately leave them unset (not just tests).
- Don't make a field `Option<T>` to accommodate tests. If every production caller passes `Some(...)` and the field is `Option` solely so a test fixture can skip wiring it, the field belongs as `T` (required). Tests that need a stripped-down loop should provide a real value or use a smaller dedicated fixture, not push `Option` onto the production type.
- Prefer raw string literals (`r#"..."#`) for any multi-line text — LLM prompts, error messages, code templates, doc snippets — so embedded `"` and newlines stay literal. `\n`/`\"` escapes hurt readability and a stray `\\n` in an escape-heavy block becomes a silent prompt bug. For substitutions, lift the body into a `const` raw string and apply `String::replace("{{placeholder}}", value)` rather than building the whole text inside `format!(...)`.
- **Locks**: always use `parking_lot::Mutex` / `parking_lot::RwLock`, never `std::sync::Mutex` / `RwLock`. `parking_lot` doesn't poison, so locking is infallible — no `.lock().unwrap()` or `.lock().expect("…poisoned")` (both of which violate the no-`.unwrap()/.expect()` rule above). `parking_lot` is already a workspace dep.
- **Concurrent maps**: reach for `dashmap` when a shared map is hot enough that a single `parking_lot::Mutex<HashMap>` would actually contend — many concurrent readers/writers, sustained high rate, large entry count. For supervisor-style registries that see a handful of ops/sec on a few-thousand-entry map (µs critical sections), `Arc<parking_lot::Mutex<HashMap>>` is simpler and just as fast. Don't reach for DashMap reflexively just because the map is shared. When you do use DashMap, remember the iter-while-mutate footgun: an iterator holds shard locks, so any `insert`/`remove` against the same shard inside the loop deadlocks — snapshot keys/values to a `Vec` first if you need to mutate.
- **CLI commands are shell-only by default, not slash.** A new `baybo` subcommand must NOT be exposed as a `/slash` command unless explicitly requested — slash dispatches inside an already-live process with no TTY, so interactive flows, live confirms, and process-lifecycle commands don't belong there. To keep a command out of slash in `crates/cli/src/slash.rs`: add its top-level name to the `matches!` exclusion in `commands()` (the menu) **and** give its arm in `slash_admissible()` an `Err("… run it from a shell")`. Opt a command INTO slash only when asked.

## Platform Support

Baybo targets **Unix only** (Linux and macOS — see `default = ["linux", "macos"]` in `crates/gateway/Cargo.toml`). Don't write `#[cfg(unix)]` / `#[cfg(not(unix))]` branches or stub shims for Windows. Call `libc::getuid`, `std::os::unix::fs::PermissionsExt`, `nix::sys::signal`, etc. directly. A non-Unix build failing is intentional.

## Dependency Management

- All dependency versions are managed centrally in the root `Cargo.toml` under `[workspace.dependencies]`.
- Crate `Cargo.toml` files MUST reference dependencies via `{ workspace = true }` — never hardcode a version in a crate.
- Adding a new external dep: declare it in the root `[workspace.dependencies]` first, then pull it into the crate with `dep = { workspace = true }` (add per-crate `features = [...]` only when the crate needs extras beyond the workspace default).
- Internal crates (`baybo-*`) are also listed in `[workspace.dependencies]` with `path = "crates/<name>"` and consumed via `{ workspace = true }`.
- Applies to both `[dependencies]` and `[dev-dependencies]`.
- Doctests are disabled workspace-wide via `[lib]\ndoctest = false` in every crate's `Cargo.toml` — empty doctest invocations were the dominant cost in `cargo test --workspace`. New crates MUST include this block. If you genuinely need a doctest in some crate, drop the line in just that crate.

## Architecture

Prefer generic/extensible architectures over hardcoding specific integrations. Ask clarifying questions about the desired abstraction level before implementing.

**Core design principles**:

- **Modular**: Each crate is an independent module; traits are defined within their own crate; crates interact via traits — high cohesion, low coupling
- **Extensible**: Channels, Tools, and Skills all plug in via registries
- **Domain crates own their tools**: a crate that owns a domain hosts its own `Tool` impls and depends on `baybo-tools` for the trait; `baybo-tools` carries only generic/core tools and never depends back on a domain crate (that would be a cycle)
- **Dedup at the seam, not the surface**: when two variants share a lifecycle (e.g. the mobile `direct` vs `relay` chat legs in `app/ios/ffi/src/transport.rs`), unify that lifecycle **once** and let each variant plug in through a narrow trait seam (`ChatTransport::establish`, `FrameCodec`, `SessionLeg::registry`). Hoist identical delegation/boilerplate into a generic helper rather than re-declaring it per variant. Do **NOT** collapse genuinely divergent bodies — different protocol/crypto/auth (`establish`, the blob legs, pairing-vs-login) — into one function behind an `if variant` / `match` branch: that's a false dedup that couples things that differ essentially and grows worse with each new case. Share only what is literally identical; keep the divergent parts in their own per-variant files.
- **Secure**: Encrypted secret storage, input leak detection, least-privilege networking and credential injection
- **Governable**: All Skill/Tool/extensions must carry source, version, hash, trust level, and capability declarations; selection and execution are auditable
- **Observable**: Full call-chain tracing; Job system manages all async operation states; supports session replay, trace forking and rollback; logs/traces record only sanitized placeholders and summaries
- **Reliable**: Built-in error recovery, retry, and degradation strategies
- **Actor model**: Message events decoupled from execution via Actor-based concurrency
- **Long-running**: Supports cron scheduling, workspace identity files, and daemon-style operation

All I/O is async with tokio. Use `Arc<T>` for shared state, `RwLock` for concurrent access.

## Module Design Specs

**Before working on any crate, always read its corresponding design document in `docs/modules/` first.** The design doc is the source of truth for that module's architecture, trait definitions, and implementation details. Code should follow the spec; the spec is the tiebreaker when in doubt.

Module index: [`docs/modules/README.md`](docs/modules/README.md)

## Subsystem Docs

For non-module-crate topics, read the relevant doc before touching that area:

- [`docs/webui.md`](docs/webui.md) — embedded React dashboard (`app/web/`), pnpm/Vite workflow, OpenAPI codegen, Tailwind v4 design tokens.
- [`docs/web-chat.md`](docs/web-chat.md) — web chat UI **feature** reference (`app/web/src/pages/ChatPage.tsx` + `app/web/src/pages/chat/`): conversations/folders/pin, composer + attachments + model switch, slash-command completion, input-history ring, the interjection queue, thread/turn rendering, and the WS data-flow backbone.
- [`docs/cron-groups.md`](docs/cron-groups.md) — collapsing a cron job's fire sessions into one chat-list row. The grouping is **derived** from `TriggerSource::Cron { cron_job_id }`, never a `session_folders` row — read the "Why nothing is stored" section before proposing the folder-row design again.
- [`docs/sync-protocol.md`](docs/sync-protocol.md) — chat sync protocol v2 (one cursor, one sync call, three data planes): the `sync`/point-lookup REST surface, the `SubscribeState`/`Gap` wire frames, the client sync loop + outbox, and rebase/gap handling. [`docs/CONTEXT.md`](docs/CONTEXT.md) is its terminology glossary (canonical names + retired-alias smells).
- [`docs/bench-web.md`](docs/bench-web.md) — standalone read-only viewer (`bench/bench-web`) for bench `results/` + agent `trace/` artifacts; spine model + per-bench adapters, ts-rs gate.
- [`docs/sidecars.md`](docs/sidecars.md) — embedded JS sidecars (`sidecars/channel/*`, `sidecars/tool/*`), bundling/install pipeline, domain registration, and the browser sidecar (CDDM wrapper, security trade-offs, docker mode).
- [`docs/modules/storage.md`](docs/modules/storage.md) — sqlite storage; all deletable tables use plain `DELETE` (no soft-delete tombstones).
- [`docs/fuzzing.md`](docs/fuzzing.md) — `baybo-security` cargo-fuzz harness and targets.
- [`docs/testing.md`](docs/testing.md) — test layout, `test-support` gating, shared fixtures.
- [`docs/external-commands.md`](docs/external-commands.md) — external binaries baybo shells out to (`git`/`sh`/`rg`/sandbox backends/`uv`/`bun`), required-vs-optional, and how the in-container benches provide or skip each.
