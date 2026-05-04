# Aura Development Guide

**Aura** is an intelligent assistant framework built on large language models, supporting multi-channel access, tool invocation, skill extensions, with comprehensive context management, compression, and error recovery mechanisms.

## Build & Test

```bash
cargo fmt                                                       # format
cargo clippy --all --benches --tests --examples --all-features  # lint (zero warnings)
cargo test                                                      # unit tests
RUST_LOG=aura=debug cargo run                                   # run with logging

pnpm install                                                    # hydrate TS workspaces
pnpm --filter @aura/channel-sdk test                            # SDK unit + e2e tests
scripts/check-ts-bindings.sh                                    # ts-rs regen CI gate
```

Test layout, `test-support` feature gating, and the shared fixture inventory are in [`docs/testing.md`](docs/testing.md). Read it before adding tests that cross crate boundaries.

Verbose / scoped logging:

```bash
RUST_LOG=aura=trace cargo run                # verbose
RUST_LOG=aura::agent=debug cargo run         # agent module only
```

## Code Style

- Prefer `crate::` for cross-module imports; `super::` is fine in tests and intra-module refs
- No `pub use` re-exports unless exposing to downstream consumers
- No `.unwrap()` or `.expect()` in production code (tests are fine)
- Use `thiserror` for error types in `error.rs`; map errors with context: `.map_err(|e| SomeError::Variant { reason: e.to_string() })?`
- Prefer strong types over strings (enums, newtypes); use typed structs instead of `HashMap<String, Value>`, only keep an `extra` field for truly dynamic extensions
- Keep functions focused, extract helpers when logic is reused
- Default to zero comments. Only add one when the WHY is non-obvious (hidden constraint, subtle invariant, workaround). Don't narrate WHAT the code does — well-named identifiers cover that.
- Avoid exporting unnecessary item, prefer `pub(crate)`; use `pub` only when necessary
- Test-only helpers (fakes, `NeverShutdown`-style stubs, dummy fixtures) MUST be gated so they don't ship in release builds:
  - Same-crate tests only → `#[cfg(test)]`.
  - Consumed by another crate's tests → gate with `#[cfg(any(test, feature = "test-support"))]` and add a `test-support = []` feature in `Cargo.toml`. Downstream crates pull them in via `aura-<crate> = { workspace = true, features = ["test-support"] }` in `[dev-dependencies]`.
  - Never leave a test-only item plain `pub` — "it's named `Never...`" is not a gate.

## Platform Support

Aura targets **Unix only** (Linux and macOS — see `default = ["linux", "macos"]` in `crates/gateway/Cargo.toml`). Don't write `#[cfg(unix)]` / `#[cfg(not(unix))]` branches or stub shims for Windows. Call `libc::getuid`, `std::os::unix::fs::PermissionsExt`, `nix::sys::signal`, etc. directly. A non-Unix build failing is intentional.

## Dependency Management

- All dependency versions are managed centrally in the root `Cargo.toml` under `[workspace.dependencies]`.
- Crate `Cargo.toml` files MUST reference dependencies via `{ workspace = true }` — never hardcode a version in a crate.
- Adding a new external dep: declare it in the root `[workspace.dependencies]` first, then pull it into the crate with `dep = { workspace = true }` (add per-crate `features = [...]` only when the crate needs extras beyond the workspace default).
- Internal crates (`aura-*`) are also listed in `[workspace.dependencies]` with `path = "crates/<name>"` and consumed via `{ workspace = true }`.
- Applies to both `[dependencies]` and `[dev-dependencies]`.

## Architecture

Prefer generic/extensible architectures over hardcoding specific integrations. Ask clarifying questions about the desired abstraction level before implementing.

**Core design principles**:

- **Modular**: Each crate is an independent module; traits are defined within their own crate; crates interact via traits — high cohesion, low coupling
- **Extensible**: Channels, Tools, and Skills all plug in via registries
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

- [`docs/webui.md`](docs/webui.md) — embedded React dashboard (`web/`), pnpm/Vite workflow, OpenAPI codegen, Tailwind v4 design tokens.
- [`docs/sidecars.md`](docs/sidecars.md) — embedded JS sidecars (`channel-src/*`, `tool-src/*`), bundling/install pipeline, domain registration, and the browser sidecar (CDDM wrapper, security trade-offs, docker mode).
- [`docs/modules/storage.md`](docs/modules/storage.md) — libsql storage; all deletable tables use plain `DELETE` (no soft-delete tombstones).
- [`docs/fuzzing.md`](docs/fuzzing.md) — `aura-security` cargo-fuzz harness and targets.
- [`docs/testing.md`](docs/testing.md) — test layout, `test-support` gating, shared fixtures.
