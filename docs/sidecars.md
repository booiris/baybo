# Embedded Sidecars

In-tree sidecars under `channel-src/*` and `tool-src/*` ship inside the baybo binary as zstd-compressed JS bundles. `crates/gateway/build.rs` invokes the package's `bundle` script (channels: `bun build --target=bun --minify`; tools: `esbuild`), zstd-compresses the result + any aux files, and emits `$OUT_DIR/sidecar_assets.rs`. At boot, `SidecarRuntime::install` materialises everything to `$XDG_CACHE_HOME/baybo/sidecars/<name>-<hash>/{bundle.mjs, <aux...>}` — hash-keyed so an upgrade never overwrites an old bundle, and the janitor sweeps stale `<hash>/` dirs once a day (live-set allowlist + 7d mtime guard).

- **Build prereqs**: `pnpm install` populates `node_modules`, and `bun` must be on `PATH` (channel sidecars; override with `BAYBO_BUN_BIN`). Tool sidecars run under `node` (override with `BAYBO_NODE_BIN`). Missing the install degrades to `cargo:warning=…` with empty embedded assets — the build still succeeds, the supervisor logs "embedded sidecar runtime unavailable". Set `BAYBO_REQUIRE_SIDECARS=1` for release builds to hard-fail on packaging failures.
- **Why bun for channels**: with `--target=bun`, bun substitutes its own polyfills for `ws` / `node-fetch@2` / `silk-wasm`, dodging three node + esbuild traps (DEP warnings, esbuild minifier deadlocking `bot.init()`, and `silk-wasm`'s `__filename` reference in ESM). Tool sidecars don't ship those packages and run under node directly.

Every sidecar self-declares an `baybo.domain` in its `package.json` (`channel`, `tool`, …). Domain identifies the family; the per-package name identifies an individual sidecar inside it. Constants live in `baybo_gateway::sidecar::domains`. **Build-time enforcement**: a missing or invalid domain (must match `[a-z0-9_]+`) is a hard `cargo build` error — new sidecars can't silently default into the wrong family.

Adding a new sidecar:
1. Pick a directory (`channel-src/<name>/` or `tool-src/<name>/`), add it to `pnpm-workspace.yaml`.
2. `package.json` declares `"baybo": { "domain": "channel" | "tool" | ..., "kind": "mcp_server"? }`.
3. For a new family, add a `domains::YOUR_DOMAIN` constant in `crates/gateway/src/sidecar/assets.rs` and iterate it from whichever supervisor / route / CLI dispatches that family.
4. For a new tool-domain MCP server: also add an `*_mcp_profile(...)` builder in `crates/tools/src/mcp/profile/<name>.rs` (primitive args so `baybo-tools` stays free of an `baybo-config` dep) and append an entry in `runtime::build_managers`.

Existing dispatch:
- `baybo channel list/add` filters `runtime.names_in_domain(domains::CHANNEL)`.
- `SidecarSupervisor` (channel restart loop) iterates `domains::CHANNEL`.
- `baybo_tools::mcp::embedded_servers(&profiles)` consumes the `EmbeddedMcpProfile` list from `runtime::build_managers` (today: just browser when `browser.enable=true`).

## Media side-channel (`/v1/blobs/*`)

Non-text media — a file the agent sends, a screenshot a tool produced, an image a user uploads — **never rides the channel WebSocket**. The bytes live in the gateway's content-addressed `BlobStore` (`crates/store/src/blob.rs`); the wire carries only a `WireAttachment` reference (`kind`, `blob_id`, `mime_type`, `size`, `filename`). A 100 MiB file is a tiny frame, and a slow media transfer can't head-of-line-block unrelated traffic on the same multiplexed connection. Bytes move over a separate channel-token-authenticated HTTP route, `POST/GET /v1/blobs/*` (`crates/gateway/src/channel/blobs.rs`), mounted under the same auth middleware as `/channel-ws`.

`BlobStore::put_stream` hashes chunks on the fly, writes a temp file, then renames to `<blobs_root>/<sha256[0..2]>/<sha256>`, so identical content dedups to one physical file. Each `put` still mints a **distinct, unguessable `blob_id`** (`"sha256:<hex>.<read_token>"`, see `SHA256_PREFIX`) — the id *is* the read capability, so "same bytes" ≠ "same id" and sharing an id delegates read access. The 100 MiB ceiling (`MAX_BLOB_BYTES`) is enforced incrementally during upload and mirrored by axum's `DefaultBodyLimit`.

**Outbound (agent → user).** A tool stages bytes and delivers a *reference*, never the bytes: `SendFile` (`crates/tools/src/builtin/send_local_file.rs`) streams the file through `put_stream`, then **emits it itself** via `ctx.notifier.emit_attachment(&[ContentBlock::File { blob, .. }])` and returns a plain text confirmation. The agent loop does no attachment routing — a tool that wants to deliver media to the user calls the notifier directly. `DeltaTxNotifier` (`crates/agent/src/runtime/agent_loop.rs`) turns that into an `AgentEvent::Attachment`; `agent_output_to_frame`'s `split_content` → `stat_attachment` (`crates/gateway/src/channel/adapter.rs`) turns each media `ContentBlock` into a `WireAttachment` (the `stat` fills `size` / `mime`) and ships it as `Frame::Attachment`. The sidecar then pulls the bytes back: `fetchBlob` (`sdks/channel-ts/src/blobs.ts`) does `GET /v1/blobs/<id>` with the `BAYBO_CHANNEL_TOKEN` bearer (base URL derived from `BAYBO_CHANNEL_URL`), and `sendTelegramAttachment` (`channel-src/telegram/src/media/outbound.ts`) wraps them in a grammy `InputFile`, dispatching to `sendPhoto` / `sendVideo` / `sendVoice` / `sendAudio` / `sendDocument` by `kind` + MIME. The gateway's `download` handler streams from `BlobStore::open` (no server-side buffering); the telegram side deliberately buffers via `fetchBlob` rather than `fetchBlobStream` because grammy's multipart on bun chokes on web streams (and the Bot API caps at 50 MB anyway).

`Frame::Attachment` is a standalone delivery — it carries no `role` / `ordinal` and no turn-completion meaning, so clients render it as its own bubble and must NOT fold it into, or close, an in-flight turn's work block. Sidecars handle it via `Channel.onAttachment` (NOT `onMessage`, which completes the turn): `BotChannel.onAttachment` sends the media and keeps the turn's liveness markers (typing indicator, status bubble) alive, mirroring a transient notice. Routing falls out of *where the notifier points*: on a **UserChat** turn it reaches the user channel; on **cron** there is no notifier, so `emit_attachment` is a no-op; in a **subagent** it reaches the parent's result waiter, which drops non-`Message` events (the parent gets the subagent's textual result, not raw media). The agent's terminal `Frame::Message` reply carries text only and never bundles attachments; surfaces that can't render media (the TUI) drop the `Frame::Attachment`.

**Inbound (user → agent).** The mirror image: a sidecar `POST /v1/blobs` to stage a platform attachment (`uploadBlob`), then references the returned id in the `attachments` of the `Frame::Message` it sends upstream. Uploads run a pairing gate first — `authorize_upload` checks the `(channel_type, bot_id, user_id)` triple (from the `x-baybo-bot-id` / `x-baybo-user-id` headers) against `PairingService`, so an unpaired peer can't cause durable 100 MiB writes; TUI / web / tool-sidecar uploads are session-scoped and bypass it.

## Browser tool sidecar (`tool-src/browser`)

Thin wrapper around Google's [`chrome-devtools-mcp`](https://github.com/ChromeDevTools/chrome-devtools-mcp) (CDDM, version-pinned in `tool-src/browser/package.json`). Spawned by `McpReconciler` over stdio MCP; CDDM's core tools (`navigate_page`, `click`, `take_screenshot`, `evaluate_script`, `take_snapshot`, `list_console_messages`, network observers, …) surface as `browser/<tool>` to the LLM. We disable several CDDM categories and tools to keep the agent's tool-list footprint small: `extensions` (mutates the persistent profile), `emulation` (`emulate` / `resize_page` — viewport is fixed at sidecar boot, and `emulate`'s schema is a ~1.2 KB enum we rarely need), `performance` (multi-second traces); plus name-level overrides for `lighthouse_audit`, the `webmcp` pair (`list_webmcp_tools` / `execute_webmcp_tool` — require host-side WebMCP support no normal page exposes), and `take_memory_snapshot` (debug-only). The proxy's `BLOCKED_TOOLS` set in `tool-src/browser/src/server.ts` is the source of truth for name-level overrides; category flags live in `buildArgs`. One additional in-process tool — `browser/read_page` — wraps Mozilla Readability + Turndown for clean Markdown extraction.

**Opt-in**: `baybo.json:browser.enable=false` by default — when off, the bundle stays inert in the binary and `browser/*` tools never appear. All operator-facing knobs live on the `BrowserConfig` struct (`crates/config/src/browser.rs`); the wrapper itself reads `BAYBO_BROWSER_*` env vars only as internal gateway↔child plumbing.

**Chrome binary**: auto-downloads Google Chrome for Testing 'stable' to `$XDG_CACHE_HOME/baybo/browser/chrome/` on first boot via `@puppeteer/browsers` — non-blocking (the MCP server connects immediately so the LLM sees the full tool list; calls during install get a synthetic "preparing, N% complete, retry" response via `GuardingTransport`). Pin a specific Chrome via `browser.chrome_path`. Pre-warm air-gapped: `pnpm --filter @baybo/tool-browser run install-chrome`.

**Auto-fontconfig**: `<workspace>/work/.fonts/` is always pinned as a Chrome fontconfig `<dir>` (synthesised temp `fonts.conf` + `FONTCONFIG_FILE` env). Drop a CJK / icon font there and Chrome picks it up next restart. macOS no-op (Chrome uses Core Text).

### Security trade-offs (deliberate)

Shape what the agent can be safely told to do — not bugs but load-bearing:

- **No per-Baybo-session isolation**. All sessions in one Baybo process share one Chrome profile (CDDM has no `BrowserContext` primitive). Concurrent sessions see each other's cookies. Docker mode does not restore isolation.
- **No in-sidecar SSRF guard**. CDDM doesn't gate navigation to RFC1918 / loopback / cloud-metadata. With approvals also off, the LLM can navigate to `http://169.254.169.254/...` un-prompted. Treat `browser/navigate_page` to a non-vetted hostname the way you'd treat a `WebFetch` to that host.
- **No per-call approval gate**. The browser MCP profile declares `capabilities = []` (`crates/tools/src/mcp/profile/browser.rs`) and CDDM emits no `_meta.baybo` annotations, so the agent loop's pre-execute approval prompt never fires for `browser/*`. Add HITL gateway-side, not in the MCP layer.
- **Wide tool surface**. `evaluate_script` runs arbitrary JS in the page and is ungated. (Performance trace, lighthouse audit, and webmcp tools are hidden by the wrapper, but the rest of CDDM's surface — including arbitrary JS eval — is live.)
- **DNS-rebinding window**. CDDM runs no sidecar-owned resolver — the agent has no checkpoint between resolution and CDP commands.

### Docker mode (`browser.docker.*`)

Opt-in via `browser.docker.enable=true`. Spawns a debian-slim container running consumer Chrome behind Xvfb and connects via CDP. Headed Chrome (no `HeadlessChrome` UA) is the point — many anti-bot stacks reject the headless profile. Any docker failure (daemon down, perms, …) transparently falls back to host-headless; boot never breaks on a missing daemon. **macOS exception**: force-disabled on darwin (Docker Desktop's hidden Linux VM defeats the "real native Chrome" point — wrapper logs an explicit "ignored on macOS" line).

- `docker.cdp_url`: escape hatch — when set, skips every docker interaction and connects directly to a pre-existing Chrome (k8s, docker-compose, remote host).
- `docker.web_vnc_port`: opt-in noVNC observability (`x11vnc` + `websockify` on the configured port). Open `http://127.0.0.1:<port>/vnc.html` in any browser. **No password, loopback-only by design** — for remote access use `ssh -L`. Don't publish on a public interface.
- `docker.image_tag`: override the deterministic `baybo-browser:<sha256(Dockerfile+entrypoint)[..12]>` tag and skip the on-first-boot build (air-gapped registries).

**Sandbox in docker mode**: `browser.sandbox` is ignored — the container is the trust boundary; Chrome runs `--no-sandbox` because the slim base ships no SUID `chrome-sandbox`. Tightening would require a custom seccomp profile + a base image with the helper installed.

**Container lifecycle**: one container per Baybo process, named `baybo-browser-<pid>-<rand6>`, labelled `baybo.role=browser-sidecar` + `baybo.pid=<pid>`. `docker run` is **not** invoked with `--rm` (so a startup crash leaves logs fetchable via `docker logs`). Cleanup: success-path `stop()` (`docker stop -t 5` + `docker rm -f`), failure-path force-remove, and the next-boot `sweepStaleContainers()` for `kill -9` cases. Inside the container Chrome listens on loopback only (Chrome 134+ silently ignores `--remote-debugging-address=0.0.0.0` for DNS-rebinding protection); a `socat 0.0.0.0:9223 → 127.0.0.1:9222` relay forwards the published port. CDP publishes on `127.0.0.1::9223` (ephemeral host port).

**Profile portability**: `browser.profile_dir` is bind-mounted at `/data/profile`; container runs `--user $(id -u):$(id -g)` so files round-trip cleanly between host-headless and docker modes under the same operator UID.
