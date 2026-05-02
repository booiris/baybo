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

The browser sidecar is a **thin wrapper around `chrome-devtools-mcp`** (Google's own MCP server, pinned at `tool-src/browser/package.json` to a fixed version). `src/server.ts` calls `chrome-devtools-mcp`'s `createMcpServer(args, options)` programmatically with hardened flags and connects a `StdioServerTransport`. The 33 tools chrome-devtools-mcp exposes (navigation: `navigate_page`, `new_page`, `select_page`, `wait_for`, `close_page`, `list_pages`; input: `click`, `drag`, `fill`, `fill_form`, `handle_dialog`, `hover`, `press_key`, `type_text`, `upload_file`; debugging: `evaluate_script`, `get_console_message`, `lighthouse_audit`, `list_console_messages`, `take_screenshot`, `take_snapshot`; performance: `performance_analyze_insight`, `performance_start_trace`, `performance_stop_trace`; network: `get_network_request`, `list_network_requests`; emulation: `emulate`, `resize_page`; memory: `take_memory_snapshot`) surface as `browser/<tool>` to the LLM. The `extensions` category (`install_extension`, `uninstall_extension`, …) is forced **off** in the wrapper since those mutate the persistent profile.

- **Transport**: stdio MCP. The gateway's `McpReconciler` spawns `node /path/to/bundle.mjs` as a child and exchanges JSON-RPC frames over its stdin/stdout. Override the node binary with `AURA_NODE_BIN=/path/to/node`. Channel sidecars are unchanged and still use `AURA_BUN_BIN`.
- **Lifecycle**: `runtime::build_managers` materialises the embedded bundle (`SidecarRuntime::install` → `$XDG_CACHE_HOME/aura/sidecars/browser-<hash>/{bundle.mjs, cddm/build/...}`), synthesises an `EmbeddedMcpServer` entry via `aura_gateway::embedded_servers`, and hands it to `McpReconciler::new`. The reconciler restarts crashed embedded children with the supervisor's old exponential backoff (500ms → 30s, reset on success).
- **Telemetry off**: belt-and-braces. `src/server.ts` passes `usageStatistics: false` and `performanceCrux: false` programmatically; the Rust profile builder also sets `CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS=1` and `CI=1` in the child env so the CDDM CLI's env-var override path also evaluates to "off" if a future upgrade rejiggers the flag plumbing.
- **No per-call approval gate**: the browser MCP profile declares `capabilities = []` (see `crates/tools/src/mcp/profile/browser.rs`) and chrome-devtools-mcp emits no `_meta.aura` annotations, so `McpTool::accessed_resources()` returns `[]` for every browser tool call and the agent loop's pre-execute approval gate never fires. Browser tool calls are *trusted by the embedded sidecar*; if you need per-call HITL on browser actions, add it gateway-side, not in the MCP layer.
- **Image content → vision-capable LLM**: chrome-devtools-mcp's `take_screenshot` returns standard MCP `attachImage` content for screenshots <2MB (above that it writes to a host-side temp file and returns the path as text). The <2MB path goes through `aura_tools::mcp::content_adapter`, which decodes the base64 into the gateway's `BlobStore` and surfaces a `ToolOutput::MultiModalText` (16 MiB cap per image; oversize captures rejected pre-decode).
- **Bundling**: `tool-src/browser/esbuild.config.mjs` does **not** re-bundle chrome-devtools-mcp. CDDM ships pre-bundled by rollup as ~280 ESM files (Lighthouse + Puppeteer + MCP SDK rolled in). esbuild bundles only our wrapper (`src/server.ts` + `@modelcontextprotocol/sdk` inlined → `dist/bundle.mjs`, ~90 KB) and a sibling `dist/cddm/build/` tree is `cpSync`'d verbatim from `node_modules/chrome-devtools-mcp/build/`. Both ship into the Aura binary: the wrapper as the `bundle_zst` of the `browser` `SidecarAsset`, the cddm tree as recursive aux assets (declared in `package.json`'s `aura.auxAssets[].recursive=true`; build.rs walks the dir at compile time and emits one `SidecarAuxAsset` per file).
- **Chrome binary** (auto-installed on first boot, non-blocking): we install **Google Chrome for Testing** — Google's own Chrome stable build packaged for automation (same Blink/V8/codecs/Widevine as consumer Chrome, minus auto-update + sign-in). Not Chromium (the open-source upstream). chrome-devtools-mcp uses Puppeteer, which doesn't bundle Chrome. On first boot, if no Chrome is found under Aura's cache, `tool-src/browser/src/server.ts` kicks off a download of Chrome for Testing 'stable' into `$XDG_CACHE_HOME/aura/browser/chrome/<platform>/chrome-<buildId>/` via `@puppeteer/browsers` **in the background** — the MCP server connects immediately so the gateway sees the full `browser/*` tool list and the LLM can plan around it. Any `tools/call` that arrives while the download is in progress is intercepted by `GuardingTransport` (a thin wrapper around `StdioServerTransport`) and answered with a synthetic `"Browser is still being prepared … N% complete. Please retry in a few seconds."` MCP response with `isError: true` — actionable progress, not a failure. Once the download completes the wrapper mutates `args.executablePath` in place; CDDM's `getContext()` re-reads `serverArgs.executablePath` on every tool call, so the next attempt picks up the new path with no restart. Subsequent boots find the cached binary synchronously and start in `install_state=ready`. Operators with a specific Chrome pin via `aura.json:browser.chrome_path` — that branch skips the cache lookup entirely. Pre-warming for air-gapped / CI: `pnpm --filter @aura/tool-browser run install-chrome` runs the same code path as the auto-install and writes to the same cache dir, so a successful pre-warm makes the boot-time check hit the cached-path branch.
- **Boot config** (`aura.json` `browser` block — `BrowserConfig` lives in `crates/config/src/browser.rs`; the matching profile builder `aura_tools::mcp::browser_mcp_profile` lives in `crates/tools/src/mcp/profile/browser.rs` and takes primitive args so `aura-tools` and `aura-config` don't form a cycle). Operator-facing knobs live **only** in `aura.json`; the corresponding `AURA_BROWSER_*` env vars the wrapper reads are internal IPC plumbing between the gateway and the sidecar child, not a public configuration interface:
  - `enable: bool` (default `false`, **opt-in**) — master switch. When false, the sidecar is never spawned, the `browser/*` tools never appear in the registry, and the bundle stays inert in the binary. Flip to `true` to opt the agent into web browsing.
  - `chrome_path: Option<PathBuf>` — optional override for the Chrome binary. Default behaviour auto-downloads Chrome for Testing to `$XDG_CACHE_HOME/aura/browser/chrome/` and uses that; set this only to pin to a specific Chrome (system binary, custom build, air-gapped vendor distribution).
  - `sandbox: bool` (default `false`) — Chrome's renderer sandbox. Off by default because most container/CI hosts can't satisfy Chrome's user-namespace prerequisites and the browser otherwise refuses to start. Flip to `true` once you've verified the host supports the sandbox (typical Linux desktop, rootful docker with the `chrome-sandbox` SUID binary in place, …) — the renderer sandbox is the floor between attacker-controlled page code and the gateway user, so turn it on whenever the host allows. Has no effect when `enable=false`.
  - `width: u32` / `height: u32` (default `1920` / `1080`) — initial viewport in CSS pixels. Pinned per-tab on launch. Override for mobile emulation (e.g. `390` × `844`) or denser layouts. Headless practical max is 3840 × 2160.
  - `profile_dir: Option<PathBuf>` — overrides the default `<workspace_root>/work/browser/profile`. Persistent across Aura restarts (cookies / localStorage retained); lives under `work/` so it follows `workspace.path` and inherits the same gitignore + lifecycle as other `work/` state. In docker mode this is bind-mounted at `/data/profile` inside the container, so the same path round-trips across host-headless and docker modes (operator UID stays the owner via `--user $(id -u):$(id -g)`). Only one Aura process can drive a given profile dir at a time (chrome-devtools-mcp serialises browser access).
  - `docker: BrowserDockerConfig` (default: all-off) — opt-in switch to run Chrome inside a Docker container with an Xvfb display, so Chrome presents as **non-headless** (better simulates a real user, dodges `HeadlessChrome` fingerprint checks). Sub-fields `enable: bool`, `cdp_url: Option<String>`, `web_vnc_port: Option<u16>`, `image_tag: Option<String>`. See the **Docker mode** subsection below.

  These flow through `aura_gateway::collect_profiles(runtime, &config)` into the reconciler's `extra_env` map (vault entries under `mcp.browser.env` still take precedence on collision). Inside the TS sidecar, the corresponding `AURA_BROWSER_CHROME_PATH` / `AURA_BROWSER_NO_SANDBOX` / `AURA_BROWSER_VIEWPORT` (`<W>x<H>`) / `AURA_BROWSER_PROFILE_DIR` / `AURA_BROWSER_DOCKER_*` env vars are read at startup as the parent→child plumbing detail.

  **Auto-fontconfig** (no operator knob, hardcoded): `collect_profiles` always pins `<workspace_root>/work/.fonts/` as an extra Chrome fontconfig `<dir>`. Drop a CJK / icon font into that directory and the next Aura restart picks it up — no `fc-cache`, no system-level install. Chrome itself takes no font-dir flag; the plumbing is `AURA_BROWSER_EXTRA_FONT_DIRS=<work>/.fonts` → TS sidecar synthesises a temp `fonts.conf` (`<include ignore_missing>` `/etc/fonts/fonts.conf` + `<dir><work>/.fonts</dir>`) → `FONTCONFIG_FILE` env → puppeteer-spawned Chrome inherits it. In docker mode the same dir is bind-mounted at `/data/fonts` and the container's entrypoint synthesises an equivalent fonts.conf inside the container (so the operator's font drops keep working without a host-side fontconfig dance). macOS Chrome uses Core Text and ignores fontconfig, so the override is a no-op there (not a bug — there's nothing to fix; macOS Chrome just reads system fonts directly).

### Docker mode (`browser.docker.*`)

When `browser.docker.enable=true`, the sidecar spawns a Docker container running Chrome behind Xvfb and connects via CDP (`args.browserUrl=http://127.0.0.1:<published>`) instead of launching Chrome on the host. Headed Chrome (no `HeadlessChrome` UA, real GPU/window/timers) is the point — many anti-bot stacks reject the headless profile.

Knobs on `browser.docker.*` (all serde defaults are `false`/None — the substruct is fully opt-in):

- `enable: bool` (default `false`) — master switch. `true` → try docker; `false` → today's host-headless flow exactly. Flipping to `true` doesn't change behaviour on a host without docker: the wrapper logs `docker mode unavailable: <reason>; falling back to host-headless` and **falls back gracefully**. Boot never fails on a missing daemon. **macOS exception**: docker mode is force-disabled on darwin (Docker Desktop runs Linux containers in a hidden VM, so the in-container Chrome would be Linux Chrome behind that VM — not native macOS Chrome, defeating the point of the switch). macOS operators always get host-headless with native Chrome regardless of `docker.enable`; the wrapper logs an explicit "ignored on macOS" line so the override is visible.
- `cdp_url: Option<String>` (default unset) — escape hatch. When set, the sidecar **skips every docker interaction** (no `docker info`, no `docker build`, no `docker run`) and connects directly to the operator's pre-existing Chrome at this URL. Use when you're running Chrome under k8s, docker-compose, or a remote host. Takes precedence over `enable` — `cdp_url` set means "I'm managing the browser; don't touch Docker."
- `web_vnc_port: Option<u16>` (default unset) — when set, the container runs `x11vnc` (loopback-only on internal `:5900`) + `websockify` + the bundled noVNC HTML client on this port. Operator opens `http://127.0.0.1:<n>/vnc.html` in any browser to watch the agent live — no native VNC client install required. Browser-only by design (no raw-VNC port is exposed): easier to remote-tunnel (`ssh -L 6080:127.0.0.1:6080 host` then `http://127.0.0.1:6080/vnc.html`), and the operator's existing browser is the client. **No password, by design.** Host docker-port mapping pins to 127.0.0.1; the protection is "loopback only on host + SSH tunnel for remote access". Don't publish on a public interface.
- `image_tag: Option<String>` (default unset) — override the deterministic image tag. Default flow computes `aura-browser:<sha256(Dockerfile + entrypoint.sh + chrome_version)[:12]>` and `docker build`s the image on first boot if it's not already present locally. Aura version bumps that change the Dockerfile / entrypoint / Chrome pin land on a new tag → automatic rebuild. Set `image_tag` only to point at a hand-rolled image (air-gapped registry mirror, custom Chrome build); the wrapper trusts the tag exists and skips the build.

Sandbox semantics in docker mode: the `browser.sandbox` knob is **ignored**. The container is the trust boundary; Chrome runs with `--no-sandbox` because the slim base doesn't ship the SUID `chrome-sandbox` helper and rootless Chrome can't open user namespaces under the default Docker seccomp profile. Tightening this would require a custom seccomp profile + a base image with `chrome-sandbox` set up — doable but out of scope for the v1 rollout.

Container lifecycle: one container per Aura process, named `aura-browser-<pid>-<rand6>`, labelled `aura.role=browser-sidecar` + `aura.pid=<pid>`. `docker run --rm` → the container deletes itself on stop; SIGTERM on Aura runs `docker stop -t 5 <name>` first. As belt-and-braces against `kill -9` Aura crashes, the next-boot wrapper sweeps `aura.role=browser-sidecar` containers whose `aura.pid` label points at a dead PID. The container publishes port 9222 on `127.0.0.1::9222` (ephemeral host port chosen by docker, read back via `docker port`); multiple Aura instances on one host don't collide.

Profile portability: the host's `browser.profile_dir` (default `<workspace_root>/work/browser/profile`) is bind-mounted at `/data/profile`. The container is `--user $(id -u):$(id -g)`-overridden so files written there stay owned by the host operator's UID — same operator UID round-trips cleanly between host-headless and docker modes. Cross-UID profile reuse breaks (root-mode Aura writes a profile, user-mode Aura can't read it).

Image: `tool-src/browser/docker/Dockerfile` (debian:bookworm-slim base + Xvfb/x11vnc/tini/CJK fonts/Chrome runtime deps + Chrome for Testing pinned via `--build-arg CHROME_VERSION=<resolved>`) and `tool-src/browser/docker/entrypoint.sh` ship as `recursive: true` aux assets in `package.json:aura.auxAssets`. They materialise next to the bundle at `$XDG_CACHE_HOME/aura/sidecars/browser-<hash>/docker/{Dockerfile,entrypoint.sh}` and the wrapper points `docker build` at that dir. Air-gapped operators: pre-build the image, set `browser.docker.image_tag` to that tag, and the wrapper skips the build entirely (network-free boot).

### What the replacement removed (vs. the old Playwright-based sidecar)

We previously shipped a hand-written Playwright wrapper with a curated 12-tool surface, in-sidecar SSRF guard, per-Aura-session `BrowserContext` isolation, and `_meta.aura.access_rule` per-call approval annotations. The chrome-devtools-mcp swap is intentionally a thinner trust layer — operators get the upstream tool surface and depend on Aura's outer trust boundaries:

- **Per-Aura-session isolation gone**. All sessions in one Aura process share one Chrome profile. chrome-devtools-mcp has no per-session `BrowserContext` primitive. If two Aura sessions navigate concurrently, they see each other's cookies. (Docker mode does not restore per-session isolation either — the container hosts a single Chrome with the same shared profile, just behind Xvfb.)
- **In-sidecar SSRF guard gone**. CDDM doesn't gate navigation to RFC1918 / loopback / cloud-metadata IPs. With approvals also off, the LLM can navigate to `http://169.254.169.254/...` un-prompted. If the host has nothing else blocking egress, this is reachable.
- **Tool surface 12 → 33**, with `lighthouse_audit`, full perf tracing, `evaluate_script` (arbitrary JS in the page), `take_memory_snapshot`. The wrapper hides the `extensions` category (force-`false` in `src/server.ts`) since those mutate the persistent profile.
- **DNS rebinding window remains**, with no agent-side checkpoint anymore. CDDM does not run a sidecar-owned resolver. Treat any `browser/navigate` to a non-vetted hostname the way you'd treat a `WebFetch` to that host.

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
