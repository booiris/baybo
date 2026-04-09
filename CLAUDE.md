# Aura Development Guide

**Aura** is an intelligent assistant framework built on large language models, supporting multi-channel access, tool invocation, skill extensions, with comprehensive context management, compression, and error recovery mechanisms.

## Build & Test

```bash
cargo fmt                                                      # format
cargo clippy --all --benches --tests --examples --all-features  # lint (zero warnings)
cargo test                                                     # unit tests
RUST_LOG=aura=debug cargo run                                  # run with logging
```

## Code Style

- Prefer `crate::` for cross-module imports; `super::` is fine in tests and intra-module refs
- No `pub use` re-exports unless exposing to downstream consumers
- No `.unwrap()` or `.expect()` in production code (tests are fine)
- Use `thiserror` for error types in `error.rs`
- Map errors with context: `.map_err(|e| SomeError::Variant { reason: e.to_string() })?`
- Prefer strong types over strings (enums, newtypes); use typed structs instead of `HashMap<String, Value>`, only keep an `extra` field for truly dynamic extensions
- Keep functions focused, extract helpers when logic is reused
- Comments for non-obvious logic only
- Avoid exporting unnecessary item, prefer `pub(crate)` for functions and structs; use `pub` only when necessary

## Maintainability

- Keep visibility minimal. If an item is not used outside its module or crate, prefer private visibility or `pub(crate)` over `pub`.
- Regularly check for exported dead code with `scripts/check-unused-pub.sh`, which installs or updates `cargo-workspace-unused-pub` and detects `pub` items that are not actually used anywhere in the workspace.
- Treat unused `pub` items as maintainability debt: remove them when obsolete, or reduce their visibility if external exposure is unnecessary.

## Architecture

Prefer generic/extensible architectures over hardcoding specific integrations. Ask clarifying questions about the desired abstraction level before implementing.

**Core design principles**:

- **Modular**: Each crate is an independent module; traits are defined within their own crate; crates interact via traits — high cohesion, low coupling
- **Extensible**: Channels, Tools, and Skills all plug in via registries; Tool/extensions loaded through WASM runtime
- **Secure**: Encrypted secret storage, input leak detection, layered execution isolation (WASM by default, container sandbox as fallback), least-privilege networking and credential injection
- **Governable**: All Skill/Tool/extensions must carry source, version, hash, trust level, and capability declarations; selection and execution are auditable
- **Observable**: Full call-chain tracing; Job system manages all async operation states; supports session replay, trace forking and rollback; logs/traces record only sanitized placeholders and summaries
- **Reliable**: Built-in error recovery, retry, and degradation strategies
- **Actor model**: Message events decoupled from execution via Actor-based concurrency
- **Long-running**: Supports heartbeat, background routines, workspace identity files, and daemon-style operation

All I/O is async with tokio. Use `Arc<T>` for shared state, `RwLock` for concurrent access.

## Debugging

```bash
RUST_LOG=aura=trace cargo run                # verbose
RUST_LOG=aura::agent=debug cargo run         # agent module only
```

## Module Design Specs

**Before working on any crate, always read its corresponding design document in `docs/modules/` first.** The design doc is the source of truth for that module's architecture, trait definitions, and implementation details. Code should follow the spec; the spec is the tiebreaker when in doubt.

Module index: `docs/modules/README.md`
