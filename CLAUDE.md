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

## Channel Sidecars (embedded)

Every in-tree channel sidecar under `channel-src/*` ships inside the aura binary as a zstd-compressed JS bundle. `crates/gateway/build.rs` runs `pnpm --filter <pkg> bundle` (which invokes `bun build --target=bun --minify` to emit a single self-contained file at `dist/bundle.mjs`), zstd-compresses each bundle plus any aux assets, and emits `$OUT_DIR/sidecar_assets.rs`. At boot, `SidecarRuntime::install` materialises everything to `$XDG_CACHE_HOME/aura/sidecars/<channel>-<hash>/{bundle.mjs, <aux...>}` (hash-keyed so upgrades never overwrite the old files — a downgraded install can still find its bundle). `SidecarSupervisor` then runs one restart loop per embedded channel type and spawns each via the host's `bun` (resolved from `PATH`, override with `AURA_BUN_BIN`).

- Pre-reqs before `cargo build`: `pnpm install` must have populated `channel-src/*/node_modules` and `sdks/channel-ts/dist`, and `bun` must be on `PATH` (used both at build time by the `bundle` script and at runtime to execute the bundle). Missing the install degrades to `cargo:warning=…` and empty embedded assets — the build still succeeds, the supervisor logs "embedded sidecar runtime unavailable".
- Adding a new sidecar: create `channel-src/<name>/` with `src/index.ts` calling `runSidecar`, add it to `pnpm-workspace.yaml`, copy the `"bundle": "bun build …"` script from one of the existing sidecars, and run `pnpm install`. `build.rs` picks it up automatically via the `channel-src/*` enumeration — no Rust changes needed.
- Why bun (not node + esbuild): with `--target=bun`, bun's bundler substitutes its own polyfills for the npm packages our sidecars pull in transitively (`ws` → bun's WHATWG WebSocket, `node-fetch@2` → bun's native fetch, etc.). That sidesteps several traps the node + esbuild combo hits: `node-fetch@2` + `whatwg-url@5` emit `DEP0040`/`DEP0169` warnings on every Bot API call; esbuild's minifier mangles `node-fetch@2`'s async stack badly enough that `bot.init()` deadlocks; `silk-wasm`'s dual-package `main: lib/index.cjs` references `__filename` which doesn't exist in ESM scope. bun ducks all three by replacing the offending packages outright at bundle time.
- Forcing a strict release build: set `AURA_REQUIRE_SIDECARS=1` to turn any sidecar packaging failure into a hard `cargo build` error (the gateway boots fine without sidecars during local backend hacking, but a release build with no embedded channels is almost always a packaging mistake).

## Sidecar Domains (recently redesigned)

Every embedded sidecar self-declares a **domain** in its `package.json`:

```json
"aura": { "domain": "channel" }   // telegram, weixin, …
"aura": { "domain": "browser" }   // browser
```

Domain is a free string; well-known ones live as constants in `aura_gateway::sidecar::domains` (`CHANNEL`, `TOOL`). Domain identifies the *family* (channel, tool, …); the per-package name (telegram, weixin, browser, …) identifies an individual sidecar inside that family. Adding a new family is just declaring a new domain in a new sidecar's package.json — no enum, no runtime, no CLI changes required:

```rust
SidecarRuntime::install()                             // materialise every embedded bundle
    .domains()                                        // ["channel", "tool", …]
    .names_in_domain(domains::CHANNEL)                // ["telegram", "weixin"]
    .names_in_domain(domains::TOOL)                   // ["browser", … future tool sidecars]
    .domain_of("telegram")                            // Some("channel")
    .domain_of("browser")                             // Some("tool")
    .bundle_for("browser")                            // Some(<materialised path>)
```

Adding a new family / domain:
1. Pick a directory (existing `tool-src/` or a new `<x>-src/`) and add it to `pnpm-workspace.yaml` and `crates/gateway/build.rs`.
2. Add a sidecar package with `"aura": { "domain": "your_domain" }`.
3. Add a constant `pub const YOUR_DOMAIN: &str = "your_domain"` to `crates/gateway/src/sidecar/assets.rs::domains` (optional but recommended to avoid typos).
4. Iterate `runtime.names_in_domain(domains::YOUR_DOMAIN)` in whatever supervisor / route / CLI code dispatches that family.

Adding a new tool-domain MCP server (no new family — drops in alongside `browser`):
1. Create `tool-src/<name>/` with `"aura": { "domain": "tool", "kind": "mcp_server" }` in its package.json, an MCP-SDK entry at `src/server.ts`, and an esbuild config that emits `dist/bundle.mjs`.
2. Add a corresponding config block (e.g. `your_tool: YourToolConfig` on `AuraConfig`).
3. Add a `*_mcp_profile(...)` builder in `crates/tools/src/mcp/profile/<name>.rs` (taking primitive args so `aura-tools` stays free of an `aura-config` runtime dep), and append a corresponding entry in `runtime::build_managers` that resolves the bundle path via `SidecarRuntime::bundle_for(...)` and unpacks the config block.

The existing surfaces wire to specific domains:
- `aura channel list/add` filters `runtime.names_in_domain(domains::CHANNEL)` — never sees other domains.
- `SidecarSupervisor` (channel restart loop) iterates `domains::CHANNEL`.
- `aura_tools::mcp::embedded_servers(&profiles)` consumes the `EmbeddedMcpProfile` list `runtime::build_managers` builds (today: just the browser profile when `browser.enable=true`). The profiles + builders live in `aura-tools` next to the MCP machinery; the gateway's role is the two host-tool lookups (`aura_gateway::node_binary()` for the spawn command, `SidecarRuntime::bundle_for("browser")` for the `dist/bundle.mjs` path).

The directory layout (`channel-src/*`, `tool-src/*`) is just file-system organisation; runtime classification is by `aura.domain`. A new domain doesn't have to live under a domain-named directory.

Build-time enforcement: a sidecar without `aura.domain` (or with an invalid one — must match `[a-z0-9_]+`) is a hard `cargo build` error. New sidecars can't silently default into the wrong family.

## Tool-domain Sidecars (`tool-src/*`)

Tool sidecars live under `tool-src/*` and declare `"aura": { "domain": "tool", "kind": "mcp_server" }`. Today the only tool sidecar is `browser` (server name `browser` → LLM tools surface as `browser/<tool>`); future siblings (code_exec, db_query, …) sit under the same `tool` domain with their own server names.

The browser sidecar runs as an **embedded stdio MCP server** — same protocol any user-configured `.mcp.json` server would use. The tool surface (12 tools: `navigate`, `snapshot`, `click`, `type`, `scroll`, `back`, `press`, `get_images`, `screenshot`, `console`, `dialog`, `cdp`) is declared entirely on the TS side at `tool-src/browser/src/tools.ts`; Aura ingests them through the regular MCP reconciler, which namespaces them as `browser/<tool>` (e.g. the LLM sees `browser/navigate`, not `browser_navigate`).

- **Transport**: stdio MCP. The gateway's `McpReconciler` spawns `node /path/to/bundle.mjs` as a child and exchanges JSON-RPC frames over its stdin/stdout. Override the node binary with `AURA_NODE_BIN=/path/to/node`. Channel sidecars are unchanged and still use `AURA_BUN_BIN`.
- **Lifecycle**: `runtime::build_managers` materialises the embedded bundle (`SidecarRuntime::install` → `$XDG_CACHE_HOME/aura/sidecars/browser-<hash>/bundle.mjs`), synthesises an `EmbeddedMcpServer` entry via `aura_gateway::embedded_servers`, and hands it to `McpReconciler::new`. The reconciler restarts crashed embedded children with the supervisor's old exponential backoff (500ms → 30s, reset on success). User-configured MCP servers keep their slower "reconnect on next config-edit tick" semantics (out of scope for this change).
- **Per-tool annotations**: each tool descriptor carries `_meta.aura.{access_rule, call_label, inject_session_id_as}` (see `tool-src/browser/src/tools.ts`). `aura_tools::mcp::McpTool` reads `_meta.aura` to gate per-call approval (`screenshot` always prompts, `console` prompts only when `expression` is non-empty, `navigate` prompts only when the URL is a literal public IP) and to inject `ctx.session_id` as `context_id` so the LLM never sees session ids in the schema. Vanilla MCP servers without `_meta.aura` keep the historical "one resource for the whole server" approval semantics.
- **Generated TS types**: the `_meta.aura.*` shape (`AuraToolMeta`, `AccessRule`, `CallLabelRule`, `ResourceKind`) is generated from Rust by ts-rs (see `crates/tools/src/mcp/access_rule.rs` `#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]`). The `tool-src/browser/src/generated/` directory holds the emitted `.ts` files; `tools.ts` imports them so a wire-shape change in Rust is a TS compile error here. Regenerate via `scripts/check-ts-bindings.sh` (the same script gates `aura-channels` bindings under `sdks/channel-ts/src/generated/`).
- **Image content → vision-capable LLM**: `aura_tools::mcp::content_adapter` decodes `RawContent::Image` from any MCP server (not just browser) into the gateway's `BlobStore` and surfaces it as `ToolOutput::MultiModalText`. 16 MiB cap per image; oversize captures are rejected before the base64 decode. Audio / Resource / ResourceLink still elide for now.
- **Bundling**: `tool-src/browser/esbuild.config.mjs` produces a self-contained `dist/bundle.mjs` from `src/server.ts` — playwright + `@modelcontextprotocol/sdk` inlined. Two non-trivial workarounds documented in the config: (1) a `createRequire` + `__dirname` banner so plain Node ESM can satisfy the bundled CommonJS modules' built-in lookups; (2) an `onLoad` plugin that rewrites the one `require.resolve("../../../package.json")` call playwright-core makes at module load time (used only for stack-trace prefix filtering), since esbuild can't statically inline that path.
- **Browser tool prereq**: Playwright's bundled Chromium must be installed once: `pnpm --filter @aura/tool-browser exec playwright install chromium`. Override with `AURA_CHROMIUM_BIN` to point at a system binary; pass through via `BrowserMcpEnv.chromium_executable` if you need to wire it from gateway config.
- **Isolation guarantee** (enforced in `tool-src/browser/src/manager.ts`): the agent's browser data **never** touches the user's normal Chrome/Firefox profile. The sidecar always uses Playwright's bundled Chromium with a dedicated `userDataDir` under `$XDG_CACHE_HOME/aura/browser/profile` (override via `AURA_BROWSER_PROFILE_DIR`). The manager refuses to start if `userDataDir` resolves under a platform default profile path. Per-Aura-session `BrowserContext` isolation (keyed on the auto-injected `context_id`) means cookies / localStorage / IndexedDB never bleed across sessions either.
- **Boot config** (`aura.json` `browser` block — `BrowserConfig` lives in `crates/config/src/browser.rs`; the matching profile builder `aura_tools::mcp::browser_mcp_profile` lives in `crates/tools/src/mcp/profile/browser.rs` and takes primitive args so `aura-tools` and `aura-config` don't form a cycle): the gateway reads runtime policy from config, never from process env. Five fields:
  - `enable: bool` (default `false`, **opt-in**) — master switch. When false, the sidecar is never spawned, the `browser/*` tools never appear in the registry, and the bundle stays inert in the binary. Flip to `true` to opt the agent into web browsing.
  - `sandbox: bool` (default `false` — Chromium runs **without** its renderer sandbox by default; many container/CI environments can't satisfy the user-namespace prerequisites). Flip to `true` once you've verified the host supports it. Has no effect when `enable=false`.
  - `chrome_path: Option<PathBuf>` — system Chromium binary; default uses Playwright's bundled one.
  - `profile_dir: Option<PathBuf>` — overrides `$XDG_CACHE_HOME/aura/browser/profile`.
  - `allow_loopback: bool` (default `false`, **test-only**) — admits navigations to 127.0.0.0/8 and `::1`; production must leave this off.

  These flow through `aura_gateway::embedded_servers(runtime, &config.browser)` into the reconciler's `extra_env` map (vault entries under `mcp.browser.env` still take precedence on collision). Inside the TS sidecar, the corresponding `AURA_BROWSER_*` env vars are read at startup — they exist as a parent→child plumbing detail, not as an operator-facing knob.
- **Smoke test**: `crates/gateway/tests/mcp_browser_smoke.rs` runs the real embedded MCP server end-to-end against the materialised bundle. Three tests, all gated `#[ignore]`. Run after `pnpm install && pnpm --filter @aura/tool-browser bundle && pnpm --filter @aura/tool-browser exec playwright install chromium && cargo build`:
  ```
  cargo test -p aura-gateway --test mcp_browser_smoke -- --ignored --nocapture
  ```
  - `browser_smoke_ssrf_guard_fires_via_real_mcp_server` — drives `browser/navigate` to `http://10.0.0.1/`, asserts the in-sidecar SSRF policy returns `BLOCKED_BY_SSRF_POLICY` through the MCP hop. Doesn't need Chromium (assertSafeUrl runs before manager.acquire).
  - `browser_smoke_navigate_and_snapshot_through_real_chromium` — binds a 127.0.0.1 HTTP server, sets `BrowserConfig { allow_loopback: true, sandbox: false, .. }`, navigates real Chromium, asserts the snapshot picks up an `@eN` ref for the button, then clicks via that ref. The served HTML pre-sets a `data-aura-ref="e99"` attribute, so this also confirms the nonce'd `data-aura-ref-<hex>` defeats static pre-pollution.
  - `browser_smoke_hostile_dom_is_bounded` — adversarial regression: serves a 5,000-button page, asserts the snapshot completes in seconds (not a hang) and contains the truncation marker. Catches future regressions of the page-side `walkPageSource` budget.
- **TS unit tests**: `pnpm --filter @aura/tool-browser test` runs `tool-src/browser/test/network_policy.test.mjs` against the compiled module — covers the IPv6 expander + every SSRF deny range incl. IPv4-mapped hex forms (`::ffff:7f00:1` etc.) WHATWG URL canonicalises into.

### Known limitations

- **DNS rebinding window between the sidecar's pre-flight resolve and Chromium's connect**: `tool-src/browser/src/network_policy.ts::resolveAndCheck` runs in Node and returns vetted addresses; `route.continue()` then lets Chromium do its own DNS lookup before opening the socket. An attacker controlling authoritative DNS for a hostname can return a public IP to Node and a blocked one (cloud metadata, RFC1918) to Chromium milliseconds later. `browser/navigate`'s `_meta.aura.access_rule = public_ip_in_url_param` doesn't prompt for hostnames (only literal public IPs), so the agent has no human-in-the-loop checkpoint here. Two known-correct fixes, both substantial rewrites, deferred:
  1. Run Chromium with `--host-resolver-rules` pointing at a sidecar-owned resolver that pins the addresses our `resolveAndCheck` already vetted.
  2. Replace `route.continue()` with `route.fulfill` + sidecar-owned HTTP fetch — breaks SNI / Host header semantics and is non-trivial to keep page-equivalent.
  Until one of these lands, treat browser navigation to a non-vetted hostname the way you'd treat a `WebFetch` to that hostname: SSRF is best-effort, not a security boundary against an active adversary controlling DNS.

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
