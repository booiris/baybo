# Embedded Sidecars

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
