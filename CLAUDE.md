# Aura Development Guide

**Aura** is an intelligent assistant framework built on large language models, supporting multi-channel access, tool invocation, skill extensions, with comprehensive context management, compression, and error recovery mechanisms.

## Build & Test

```bash
cargo fmt                                                      # format
cargo clippy --all --benches --tests --examples --all-features  # lint (zero warnings)
cargo test                                                     # unit tests
RUST_LOG=aura=debug cargo run                                  # run with logging
```

Test layout, `test-support` feature gating, and the shared fixture inventory
(`MemorySecretStore`, `RecordingChannel`, `StubLlm`, `gateway_with_memory_vault`,
`SessionBuilder`, `capture_tracing`, …) are documented in
[`docs/testing.md`](docs/testing.md). Read it before adding tests that cross
crate boundaries.

### Fuzzing (aura-security)

The `aura-security` crate ships a cargo-fuzz harness at `crates/security/fuzz/`
with targets for the injection detector, leak detector, sensitive-path check,
AES-GCM crypto roundtrip, and placeholder minter. The fuzz crate is excluded
from the workspace; run from inside `crates/security/`:

```bash
cargo install cargo-fuzz                                       # one-time
rustup install nightly                                         # libFuzzer needs nightly

cd crates/security
cargo +nightly fuzz list                                        # list targets
cargo +nightly fuzz run fuzz_injection_detector                 # run until crash/Ctrl-C
cargo +nightly fuzz run fuzz_leak_detector -- -max_total_time=300
```

Available targets: `fuzz_injection_detector`, `fuzz_leak_detector`,
`fuzz_sensitive_paths`, `fuzz_crypto_roundtrip`, `fuzz_placeholder`. Seed
corpora live under `crates/security/fuzz/corpus/<target>/`.

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
- Test-only helpers (fakes, `NeverShutdown`-style stubs, dummy fixtures) MUST be gated so they don't ship in release builds:
  - Same-crate tests only → `#[cfg(test)]`.
  - Consumed by another crate's tests → gate with `#[cfg(any(test, feature = "test-support"))]` and add a `test-support = []` feature in `Cargo.toml`. Downstream crates pull them in via `aura-<crate> = { workspace = true, features = ["test-support"] }` in `[dev-dependencies]`.
  - Never leave a test-only item plain `pub` — "it's named `Never...`" is not a gate.

## Platform Support

Aura targets **Unix only** (Linux and macOS — see `default = ["linux", "macos"]` in `crates/gateway/Cargo.toml`). Don't write `#[cfg(unix)]` / `#[cfg(not(unix))]` branches or stub shims for Windows. Call `libc::getuid`, `std::os::unix::fs::PermissionsExt`, `tokio::net::UnixStream`, etc. directly. A non-Unix build failing is intentional.

## WebUI (`web/`)

The admin TCP listener serves an embedded React dashboard baked into the gateway binary. Sources live at the repo root in `web/` (React 19 + TypeScript + Vite + Tailwind v4 + react-router + react-icons, neo-brutalist visual style). `crates/gateway/build.rs` walks `web/dist/` at compile time and emits `$OUT_DIR/webui_assets.rs` with one `include_bytes!` arm per asset — no runtime embedding crate (no `rust-embed`, no `mime_guess`). `api::webui::serve` is mounted as the admin router fallback so `/`, `/assets/...`, and any unmatched path resolve there while `/healthz`, `/readyz`, and `/v1/*` keep their explicit handlers.

- Ship a real dashboard: `cd web && npm ci && npm run build && cargo build --release -p aura-gateway`. The Vite output lands in `web/dist/` (gitignored) and gets embedded on the next cargo build.
- Backend-only work: `cargo build` alone still works — if `web/dist/index.html` is missing, `build.rs` writes a one-line placeholder page so the crate compiles without needing npm.
- The webui is unauthenticated on purpose. The bundle is inert HTML/JS; every privileged data path still goes through `/v1/*` and its bearer-token gate.
- Admin API types are generated: `docs/openapi.json` is produced by `aura-gateway` (utoipa) and kept in sync by `crates/gateway/tests/openapi_spec_sync.rs` (regen with `UPDATE_OPENAPI=1 cargo test -p aura-gateway --test openapi_spec_sync`). The web build runs `openapi-typescript` over that file (`npm run gen:api`, wired into `npm run build`) to emit `web/src/api/schema.d.ts`; the runtime client lives in `web/src/api/client.ts` (`openapi-fetch` with Bearer auth pre-applied). `utoipa` itself is only a dependency of `aura-gateway` — domain crates stay framework-agnostic, and new HTTP-visible fields are added by editing the mirror DTOs in `crates/gateway/src/api/dto.rs`.
- Design tokens (`--color-brand`, `--shadow-brutal*`, `--font-mono`, …) live in `web/src/index.css` under Tailwind v4's `@theme` block. Keep the heavy-border + offset-shadow aesthetic consistent when adding new components.

## Dependency Management

- All dependency versions are managed centrally in the root `Cargo.toml` under `[workspace.dependencies]`.
- Crate `Cargo.toml` files MUST reference dependencies via `{ workspace = true }` — never hardcode a version in a crate.
- Adding a new external dep: declare it in the root `[workspace.dependencies]` first, then pull it into the crate with `dep = { workspace = true }` (add per-crate `features = [...]` only when the crate needs extras beyond the workspace default).
- Internal crates (`aura-*`) are also listed in `[workspace.dependencies]` with `path = "crates/<name>"` and consumed via `{ workspace = true }`.
- Applies to both `[dependencies]` and `[dev-dependencies]`.

## Storage (libsql) — Soft Delete

All libsql-backed tables that support deletion use **soft delete**, never a hard `DELETE`. This preserves history for audit, replay, and compliance.

- Every deletable table carries a nullable `deleted_at INTEGER` column (Unix seconds; `NULL` = live row).
- Deletion = `UPDATE ... SET deleted_at = ?now WHERE ... AND deleted_at IS NULL`. Do not emit `DELETE FROM` against these tables.
- Every read (`SELECT`) MUST include `AND deleted_at IS NULL` so soft-deleted rows stay hidden. Every mutation (`UPDATE`) on a live row MUST include the same guard so you never write through a deleted row.
- Re-insertion semantics: `INSERT OR REPLACE` and `ON CONFLICT ... DO UPDATE` must reset `deleted_at` back to `NULL` so recreating a soft-deleted id revives it (see `skill_risk.rs::upsert_job` for the pattern).
- Schema changes: add the column to the `CREATE TABLE IF NOT EXISTS` in `crates/storage/src/libsql/mod.rs`.
- Tables currently covered: `sessions`, `memories`, `session_traces`, `trace_nodes`, `trace_forks`, `secrets`, `jobs`, `job_transitions`, `cron_jobs`, `cron_executions`, `skill_risk_assessments`, `skill_risk_assessment_jobs`. The only append-only table without `deleted_at` is `cost_records` (billing audit trail).

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

## Debugging

```bash
RUST_LOG=aura=trace cargo run                # verbose
RUST_LOG=aura::agent=debug cargo run         # agent module only
```

## Module Design Specs

**Before working on any crate, always read its corresponding design document in `docs/modules/` first.** The design doc is the source of truth for that module's architecture, trait definitions, and implementation details. Code should follow the spec; the spec is the tiebreaker when in doubt.

Module index: `docs/modules/README.md`
