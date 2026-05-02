# Aura Development Guide

**Aura** is an intelligent assistant framework built on large language models, supporting multi-channel access, tool invocation, skill extensions, with comprehensive context management, compression, and error recovery mechanisms.

## Build & Test

```bash
cargo fmt                                                      # format
cargo clippy --all --benches --tests --examples --all-features  # lint (zero warnings)
cargo test                                                     # unit tests
RUST_LOG=aura=debug cargo run                                  # run with logging

pnpm install                                                   # hydrate TS workspaces
pnpm --filter @aura/channel-sdk test                            # SDK unit + e2e tests
scripts/check-ts-bindings.sh                                   # ts-rs regen CI gate
```

Test layout, `test-support` feature gating, and the shared fixture inventory
(`MemorySecretStore`, `StubLlm`, `gateway_with_memory_vault`,
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
- Default to zero comments. Only add one when the WHY is non-obvious (hidden constraint, subtle invariant, workaround). Don't narrate WHAT the code does — well-named identifiers cover that. No module-level blurbs that restate the types, no docstrings on helpers whose signature already tells the story, no inline "//" notes explaining straightforward control flow.
- Avoid exporting unnecessary item, prefer `pub(crate)` for functions and structs; use `pub` only when necessary
- Test-only helpers (fakes, `NeverShutdown`-style stubs, dummy fixtures) MUST be gated so they don't ship in release builds:
  - Same-crate tests only → `#[cfg(test)]`.
  - Consumed by another crate's tests → gate with `#[cfg(any(test, feature = "test-support"))]` and add a `test-support = []` feature in `Cargo.toml`. Downstream crates pull them in via `aura-<crate> = { workspace = true, features = ["test-support"] }` in `[dev-dependencies]`.
  - Never leave a test-only item plain `pub` — "it's named `Never...`" is not a gate.

## Platform Support

Aura targets **Unix only** (Linux and macOS — see `default = ["linux", "macos"]` in `crates/gateway/Cargo.toml`). Don't write `#[cfg(unix)]` / `#[cfg(not(unix))]` branches or stub shims for Windows. Call `libc::getuid`, `std::os::unix::fs::PermissionsExt`, `nix::sys::signal`, etc. directly. A non-Unix build failing is intentional.

## WebUI (`web/`)

The admin TCP listener serves an embedded React dashboard baked into the gateway binary. Sources live at the repo root in `web/` (React 19 + TypeScript + Vite + Tailwind v4 + react-router + react-icons, neo-brutalist visual style). `crates/gateway/build.rs` walks `web/dist/` at compile time and emits `$OUT_DIR/webui_assets.rs` with one `include_bytes!` arm per asset — no runtime embedding crate (no `rust-embed`, no `mime_guess`). `api::webui::serve` is mounted as the admin router fallback so `/`, `/assets/...`, and any unmatched path resolve there while `/healthz`, `/readyz`, and `/v1/*` keep their explicit handlers.

- TS tooling is **pnpm** repo-wide (the sole lockfile is `pnpm-lock.yaml` at the repo root; workspaces are declared in `pnpm-workspace.yaml`). Never invoke `npm` — `npm ci`, `npm install`, `npm run …` all route through `pnpm` equivalents below.
- Ship a real dashboard: `pnpm install && pnpm --filter aura-web build && cargo build --release -p aura-gateway`. The Vite output lands in `web/dist/` (gitignored) and gets embedded on the next cargo build.
- Backend-only work: `cargo build` alone still works — if `web/dist/index.html` is missing, `build.rs` writes a one-line placeholder page so the crate compiles without needing pnpm.
- UI iteration (HMR): run `cargo run -- gateway start` (debug gateway on 127.0.0.1:8889) and `pnpm --filter aura-web dev` (Vite on :5173) in parallel. `web/vite.config.ts` proxies `/v1`, `/healthz`, and `/readyz` to 8889, so the browser only hits the Vite origin and no CORS config is needed. For cross-origin setups, add the Vite origin to `gateway.cors_allowed_origins` in `aura.json` and override base URL via LoginScreen → Advanced.
- Asset caching: `index.html` is served with `Cache-Control: no-cache` so bundle-hash rotations take effect on next load; hashed `/assets/*` are `immutable`. `/assets/<missing>` returns 404 (not an SPA fallback) so a stale script tag can't be served as HTML and break module loading.
- The webui is unauthenticated on purpose. The bundle is inert HTML/JS; every privileged data path still goes through `/v1/*` and its bearer-token gate.
- Admin API types are generated: `docs/openapi.json` is produced by `aura-gateway` (utoipa) and kept in sync by `crates/gateway/tests/openapi_spec_sync.rs` (regen with `UPDATE_OPENAPI=1 cargo test -p aura-gateway --test openapi_spec_sync`). The web build runs `openapi-typescript` over that file (`pnpm --filter aura-web gen:api`, wired into `pnpm --filter aura-web build`) to emit `web/src/api/schema.d.ts`; the runtime client lives in `web/src/api/client.ts` (`openapi-fetch` with Bearer auth pre-applied). `utoipa` itself is only a dependency of `aura-gateway` — domain crates stay framework-agnostic, and new HTTP-visible fields are added by editing the mirror DTOs in `crates/gateway/src/api/dto.rs`.
- Design tokens (`--color-brand`, `--shadow-brutal*`, `--font-mono`, …) live in `web/src/index.css` under Tailwind v4's `@theme` block. Keep the heavy-border + offset-shadow aesthetic consistent when adding new components.

## Embedded Sidecars

In-tree sidecars under `channel-src/*` and `tool-src/*` ship inside the aura binary as zstd-compressed JS bundles. `crates/gateway/build.rs` invokes the package's `bundle` script (channels: `bun build --target=bun --minify`; tools: `esbuild`), zstd-compresses the result + any aux files, and emits `$OUT_DIR/sidecar_assets.rs`. At boot, `SidecarRuntime::install` materialises everything to `$XDG_CACHE_HOME/aura/sidecars/<name>-<hash>/{bundle.mjs, <aux...>}` — hash-keyed so an upgrade never overwrites an old bundle, and the janitor sweeps stale `<hash>/` dirs once a day (live-set allowlist + 7d mtime guard).

- **Build prereqs**: `pnpm install` populates `node_modules`, and `bun` must be on `PATH` (channel sidecars; override with `AURA_BUN_BIN`). Tool sidecars run under `node` (override with `AURA_NODE_BIN`). Missing the install degrades to `cargo:warning=…` with empty embedded assets — the build still succeeds, the supervisor logs "embedded sidecar runtime unavailable". Set `AURA_REQUIRE_SIDECARS=1` for release builds to hard-fail on packaging failures.
- **Why bun for channels**: with `--target=bun`, bun substitutes its own polyfills for `ws` / `node-fetch@2` / `silk-wasm`, dodging three node + esbuild traps (DEP warnings, esbuild minifier deadlocking `bot.init()`, and `silk-wasm`'s `__filename` reference in ESM). Tool sidecars don't ship those packages and run under node directly.

Every sidecar self-declares an `aura.domain` in its `package.json` (`channel`, `tool`, …). Domain identifies the family; the per-package name identifies an individual sidecar inside it. Constants live in `aura_gateway::sidecar::domains`. **Build-time enforcement**: a missing or invalid domain (must match `[a-z0-9_]+`) is a hard `cargo build` error — new sidecars can't silently default into the wrong family.

Adding a new sidecar:
1. Pick a directory (`channel-src/<name>/` or `tool-src/<name>/`), add it to `pnpm-workspace.yaml`.
2. `package.json` declares `"aura": { "domain": "channel" | "tool" | ..., "kind": "mcp_server"? }`.
3. For a new family, add a `domains::YOUR_DOMAIN` constant in `crates/gateway/src/sidecar/assets.rs` and iterate it from whichever supervisor / route / CLI dispatches that family.
4. For a new tool-domain MCP server: also add an `*_mcp_profile(...)` builder in `crates/tools/src/mcp/profile/<name>.rs` (primitive args so `aura-tools` stays free of an `aura-config` dep) and append an entry in `runtime::build_managers`.

Existing dispatch:
- `aura channel list/add` filters `runtime.names_in_domain(domains::CHANNEL)`.
- `SidecarSupervisor` (channel restart loop) iterates `domains::CHANNEL`.
- `aura_tools::mcp::embedded_servers(&profiles)` consumes the `EmbeddedMcpProfile` list from `runtime::build_managers` (today: just browser when `browser.enable=true`).

## Browser tool sidecar (`tool-src/browser`)

Thin wrapper around Google's [`chrome-devtools-mcp`](https://github.com/ChromeDevTools/chrome-devtools-mcp) (CDDM, version-pinned in `tool-src/browser/package.json`). Spawned by `McpReconciler` over stdio MCP; the ~30 CDDM tools (`navigate_page`, `click`, `take_screenshot`, `evaluate_script`, `lighthouse_audit`, `take_snapshot`, perf-trace family, …) surface as `browser/<tool>` to the LLM. The `extensions` category is force-disabled in our wrapper (mutates the persistent profile). One additional in-process tool — `browser/read_page` — wraps Mozilla Readability + Turndown for clean Markdown extraction.

**Opt-in**: `aura.json:browser.enable=false` by default — when off, the bundle stays inert in the binary and `browser/*` tools never appear. All operator-facing knobs live on the `BrowserConfig` struct (`crates/config/src/browser.rs`); the wrapper itself reads `AURA_BROWSER_*` env vars only as internal gateway↔child plumbing.

**Chrome binary**: auto-downloads Google Chrome for Testing 'stable' to `$XDG_CACHE_HOME/aura/browser/chrome/` on first boot via `@puppeteer/browsers` — non-blocking (the MCP server connects immediately so the LLM sees the full tool list; calls during install get a synthetic "preparing, N% complete, retry" response via `GuardingTransport`). Pin a specific Chrome via `browser.chrome_path`. Pre-warm air-gapped: `pnpm --filter @aura/tool-browser run install-chrome`.

**Auto-fontconfig**: `<workspace>/work/.fonts/` is always pinned as a Chrome fontconfig `<dir>` (synthesised temp `fonts.conf` + `FONTCONFIG_FILE` env). Drop a CJK / icon font there and Chrome picks it up next restart. macOS no-op (Chrome uses Core Text).

### Security trade-offs (deliberate)

Shape what the agent can be safely told to do — not bugs but load-bearing:

- **No per-Aura-session isolation**. All sessions in one Aura process share one Chrome profile (CDDM has no `BrowserContext` primitive). Concurrent sessions see each other's cookies. Docker mode does not restore isolation.
- **No in-sidecar SSRF guard**. CDDM doesn't gate navigation to RFC1918 / loopback / cloud-metadata. With approvals also off, the LLM can navigate to `http://169.254.169.254/...` un-prompted. Treat `browser/navigate_page` to a non-vetted hostname the way you'd treat a `WebFetch` to that host.
- **No per-call approval gate**. The browser MCP profile declares `capabilities = []` (`crates/tools/src/mcp/profile/browser.rs`) and CDDM emits no `_meta.aura` annotations, so the agent loop's pre-execute approval prompt never fires for `browser/*`. Add HITL gateway-side, not in the MCP layer.
- **Wide tool surface**. `evaluate_script` runs arbitrary JS in the page; `lighthouse_audit` / perf-trace / memory-snapshot are all live and ungated.
- **DNS-rebinding window**. CDDM runs no sidecar-owned resolver — the agent has no checkpoint between resolution and CDP commands.

### Docker mode (`browser.docker.*`)

Opt-in via `browser.docker.enable=true`. Spawns a debian-slim container running consumer Chrome behind Xvfb and connects via CDP. Headed Chrome (no `HeadlessChrome` UA) is the point — many anti-bot stacks reject the headless profile. Any docker failure (daemon down, perms, …) transparently falls back to host-headless; boot never breaks on a missing daemon. **macOS exception**: force-disabled on darwin (Docker Desktop's hidden Linux VM defeats the "real native Chrome" point — wrapper logs an explicit "ignored on macOS" line).

- `docker.cdp_url`: escape hatch — when set, skips every docker interaction and connects directly to a pre-existing Chrome (k8s, docker-compose, remote host).
- `docker.web_vnc_port`: opt-in noVNC observability (`x11vnc` + `websockify` on the configured port). Open `http://127.0.0.1:<port>/vnc.html` in any browser. **No password, loopback-only by design** — for remote access use `ssh -L`. Don't publish on a public interface.
- `docker.image_tag`: override the deterministic `aura-browser:<sha256(Dockerfile+entrypoint)[..12]>` tag and skip the on-first-boot build (air-gapped registries).

**Sandbox in docker mode**: `browser.sandbox` is ignored — the container is the trust boundary; Chrome runs `--no-sandbox` because the slim base ships no SUID `chrome-sandbox`. Tightening would require a custom seccomp profile + a base image with the helper installed.

**Container lifecycle**: one container per Aura process, named `aura-browser-<pid>-<rand6>`, labelled `aura.role=browser-sidecar` + `aura.pid=<pid>`. `docker run` is **not** invoked with `--rm` (so a startup crash leaves logs fetchable via `docker logs`). Cleanup: success-path `stop()` (`docker stop -t 5` + `docker rm -f`), failure-path force-remove, and the next-boot `sweepStaleContainers()` for `kill -9` cases. Inside the container Chrome listens on loopback only (Chrome 134+ silently ignores `--remote-debugging-address=0.0.0.0` for DNS-rebinding protection); a `socat 0.0.0.0:9223 → 127.0.0.1:9222` relay forwards the published port. CDP publishes on `127.0.0.1::9223` (ephemeral host port).

**Profile portability**: `browser.profile_dir` is bind-mounted at `/data/profile`; container runs `--user $(id -u):$(id -g)` so files round-trip cleanly between host-headless and docker modes under the same operator UID.

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
