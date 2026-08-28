# WebUI (`app/web/`)

> This file covers the **embedded-dashboard infrastructure** (build, embed, codegen, asset serving, design tokens) and the **trace viewer's polling model** (last section). For the **chat UI's features** — conversations, folders, the composer, slash-command completion, the input-history ring, the interjection queue, thread/turn rendering, and the WebSocket data flow — see [`web-chat.md`](web-chat.md).

The admin TCP listener serves an embedded React dashboard baked into the gateway binary. Sources live in `app/web/` — the pnpm workspace package `baybo-web` (React 19 + TypeScript + Vite + Tailwind v4 + react-router + react-icons, plus recharts, react-markdown/remark-gfm/remark-math/rehype-katex, and @dnd-kit; neo-brutalist visual style). `crates/gateway/build.rs` walks `app/web/dist/` at compile time, zstd-compresses every asset, and emits `$OUT_DIR/webui_assets.rs` with one `WebAsset { …, content_zst: include_bytes!(…) }` arm per asset — no runtime embedding crate (no `rust-embed`, no `mime_guess`). `api::webui::serve` lazily decompresses each asset on first request via a `DECOMPRESSED_ASSETS: OnceLock<…>` cache. It is mounted as the admin router fallback so `/`, `/assets/...`, and any unmatched path resolve there while `/healthz`, `/readyz`, and `/v1/*` keep their explicit handlers.

- TS tooling is **pnpm** repo-wide (the root workspace's lockfile is `pnpm-lock.yaml`, with members declared in `pnpm-workspace.yaml`: `app/web`, `sidecars/*/*`, `bench/bench-web/web`; the iOS transcript webview `app/ios/web` is a standalone pnpm project with its own lockfile — not a stray). Never invoke `npm` — `npm ci`, `npm install`, `npm run …` all route through `pnpm` equivalents below.
- Ship a real dashboard: `pnpm install && pnpm --filter baybo-web build && cargo build --release -p baybo-gateway`. The Vite output lands in `app/web/dist/` (gitignored) and gets embedded on the next cargo build.
- A dashboard that won't compile is a **hard `cargo build` error**, not a `cargo:warning`. `build.rs` shells out to `pnpm --filter baybo-web build` and panics with the full `tsc`/Vite output on failure. (It used to warn and embed the placeholder, so a type error in `app/web` shipped a broken `/` behind one line of build noise for days.)
- Backend-only work without the TS toolchain: set `BAYBO_SKIP_WEBUI=1`, and `build.rs` embeds the one-line placeholder page instead of building. It gates only the dashboard build — `ensure_pnpm_install` and the sidecar bundler still shell out to `pnpm`, but both of those degrade to a `cargo:warning` when it's missing. It is the mirror of `BAYBO_REQUIRE_SIDECARS=1`, which makes the (default-lenient) sidecar pipeline strict. CI's Rust jobs (`clippy`, `test`) set it because they carry no TS toolchain; the `frontend` job is what gates `app/web`.
- The escape hatch is wired into `.vscode/settings.json` (`rust-analyzer.cargo.extraEnv`) so an in-progress `app/web` edit doesn't redden the Rust tree. `.zed/` and `.idea/` are gitignored — Zed/JetBrains users must set it themselves. Don't export it shell-wide: a terminal `cargo build` is the only thing that tells you the dashboard is broken.
- UI iteration (HMR): run `cargo run -- gateway start` (debug gateway on 127.0.0.1:8888) and `pnpm --filter baybo-web dev` (Vite on :5173) in parallel. `app/web/vite.config.ts` proxies `/v1`, `/healthz`, and `/readyz` to 8888, so the browser only hits the Vite origin and no CORS config is needed. For cross-origin setups, add the Vite origin to `gateway.cors_allowed_origins` in `baybo.json` and override base URL via LoginScreen → Advanced.
- Asset caching: `index.html` is served with `Cache-Control: no-cache` so bundle-hash rotations take effect on next load; hashed `/assets/*` are `immutable`. `/assets/<missing>` returns 404 (not an SPA fallback) so a stale script tag can't be served as HTML and break module loading.
- **Fonts.** Inter / Space Mono come from the Google Fonts CDN (`app/web/index.html`), but KaTeX's math faces are **self-hosted**: `main.tsx` imports `katex/dist/katex.min.css`, so Vite emits the font files it references into `dist/assets` (59 files, 1.03 MB — the 60th, a 3.6 KB woff2, falls under Vite's 4 KB `assetsInlineLimit` and becomes a data URI in the CSS), and `build.rs` embeds each one, so math renders on a box with no internet. Measured cost in the binary: **~855 KB** after `zstd -19` (the woff/woff2 containers barely compress; the ~500 KB of TTFs do). The woff and ttf fallbacks are dead weight for any browser that can run this React 19 bundle — every one of them has supported woff2 for years — but pruning them means shipping and maintaining a patched copy of KaTeX's stylesheet, which is the worse trade at this size. `mime_for` in `crates/gateway/build.rs` already maps `woff`/`woff2`/`ttf`, so no serving change was needed.
- The webui is unauthenticated on purpose. The bundle is inert HTML/JS; every privileged data path still goes through `/v1/*` and its bearer-token gate.
- Admin API types are generated: `docs/openapi.json` is produced by `baybo-gateway` (utoipa) and kept in sync by `crates/gateway/tests/openapi_spec_sync.rs` (regen with `UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync`). The web build runs `openapi-typescript` over that file (`pnpm --filter baybo-web gen:api`, wired into `pnpm --filter baybo-web build`) to emit `app/web/src/api/schema.d.ts`; the runtime client lives in `app/web/src/api/client.ts` (`openapi-fetch` with Bearer auth pre-applied). `utoipa` itself is only a dependency of `baybo-gateway` — domain crates stay framework-agnostic, and new HTTP-visible fields are added by editing the mirror DTOs in `crates/gateway/src/api/dto.rs`.
- Design tokens (`--color-brand`, `--shadow-brutal*`, `--font-mono`, …) live in `app/web/src/index.css` under Tailwind v4's `@theme` block. Keep the heavy-border + offset-shadow aesthetic consistent when adding new components.

- The Cron job editor is the operator surface for unattended MCP authority. It
  fetches `GET /v1/cron/mcp-tools`, groups exact operations by configured
  server, starts with no selection for ungranted jobs, and emits a complete
  `mcp_tool_grants` replacement only when the selection changes (`[]` revokes).
  A per-server bulk select always presents an explicit warning. Persisted grants
  that no longer match a live exact tool+transport tuple remain visible as stale
  and are never rebound by tool name; Save is blocked until the operator removes
  them. API helpers and merge/diff rules are covered in `cronActions.test.ts`.

## PWA

The dashboard is an installable, offline-capable app: a web app manifest, an icon set, and a service worker that precaches the shell. What that buys is a standalone window (its own icon, no address bar, its own OS task-switcher entry) and a shell that paints without the gateway — the data behind it still needs a live `/v1`, so offline means "the app opens and tells you it can't reach the gateway", not "the app works".

**Theme colour lives in two files and must match.** `<meta name="theme-color">` in `index.html` tints the browser tab; `theme_color` in the manifest tints the installed window's chrome, and is captured at install time (an already-installed app needs a reinstall to pick up a change). Neither can reference the other, nor the `index.css` token they copy — so the build compares them and fails on a mismatch rather than shipping an app whose title bar disagrees with its own tab.

It is `--color-canvas` (`#faf6ec`), **not** the brand gold. Gold was the first choice and read as too much: the rail is the only gold surface and it is 48 px down the *left* edge, so a gold top bar sat above a page that is cream everywhere it touches. Canvas makes the chrome continuous with the header strip under it, which is what a theme colour is for. `background_color` is a third, separate thing — the splash behind the icon, white to match the icon's own background.

**The static half** lives in `app/web/public/`, which Vite copies to the output root verbatim (and which `crates/gateway/build.rs` already watches, so an icon edit rebuilds the bundle). `manifest.webmanifest` is hand-written — one file, reviewable in a diff, no plugin config to read it through. `index.html` carries the `theme-color`, the icon links, and the `apple-mobile-web-app-*` pair Safari still reads instead of `display: standalone`.

**Icons** are all derived from `assets/baybo.png` — the brand mark, and the same artwork the iOS app ships as its AppIcon, so the two apps look like one product. Regenerate them from the repo root:

```bash
# Normalize the source: flatten alpha and pull the near-white (254,254,254)
# field to pure white, so a shrunk tile leaves no faint square on a white canvas.
magick assets/baybo.png -alpha remove -alpha off -fuzz 3% -fill white -opaque white /tmp/src.png
magick /tmp/src.png -fuzz 3% -trim +repage /tmp/mark.png

cd app/web/public
for n in 512 192; do magick /tmp/src.png -resize ${n}x${n} -strip -define png:compression-level=9 pwa-$n.png; done
magick /tmp/src.png -resize 180x180 -strip -define png:compression-level=9 apple-touch-icon.png
magick /tmp/src.png -resize 390x390 -background white -gravity center -extent 512x512 -strip -define png:compression-level=9 pwa-maskable-512.png

# favicon.ico — three hand-tuned sizes, see below
magick /tmp/mark.png -morphology Erode Disk:20 -resize 15x15 -background white -gravity center -extent 16x16 -strip /tmp/ico-16.png
magick /tmp/mark.png -morphology Erode Disk:10 -resize 30x30 -background white -gravity center -extent 32x32 -strip /tmp/ico-32.png
magick /tmp/mark.png -morphology Erode Disk:6  -resize 44x44 -background white -gravity center -extent 48x48 -strip /tmp/ico-48.png
magick /tmp/ico-48.png /tmp/ico-32.png /tmp/ico-16.png favicon.ico

# Grayscale palette: the mark is black on white and everything between is
# antialiasing. Cuts the four PNGs 107 KB → 35 KB (they ride into the gateway
# binary) at an RMSE of ~0.3%.
for f in pwa-512 pwa-192 pwa-maskable-512 apple-touch-icon; do
  magick $f.png -colorspace Gray -colors 64 -strip \
    -define png:compression-level=9 -define png:compression-filter=5 $f.png
done
```

Three of those numbers are not arbitrary:

- **`-resize 390x390` for the maskable.** The platform crops a maskable icon to a circle/squircle and only the centred 80% circle is guaranteed to survive, so the mark's *diagonal* is the binding constraint — not its width. At 390 the mark measures 312×258, a half-diagonal of 202 against the 204.8 the safe circle allows. A straight 512 resize would put it at 266 and lose the antenna tips.
- **`Erode Disk:N` before each favicon size.** The mark is line art whose strokes are ~14 px in a 1007 px source — under one pixel at 16, which downsamples to pale grey mush. Eroding thickens black against white by roughly the amount each size loses. The radii are eyeballed per size (20/10/6) because the right amount is a legibility judgement, not a ratio.
- **Everything is opaque white.** iOS composites `apple-touch-icon` onto nothing and a transparent one comes out black; a transparent "any" icon disappears against a dark task switcher. `background_color` in the manifest is `#ffffff` to match — the splash screen draws the icon on it, and the app's cream canvas would frame it in a visible white square.

**The service worker** is hand-rolled: `app/web/pwa/service-worker.js` (the source, with two placeholders) plus `app/web/pwa/plugin.ts` (a ~120-line Vite plugin that fills them in). `vite-plugin-pwa` was tried and reverted — it pulls workbox and, transitively, 249 packages into a workspace whose whole point is a dependency-light dashboard. Doing it here also lets the precache list be chosen against this gateway's real asset semantics instead of a glob.

The plugin runs in `closeBundle` — the one hook that fires after Vite has copied `public/` — and scans the actual `dist/` tree, so `public/` assets are in the list. It precaches `.html`, `.js`, `.css`, `.woff2`, `.svg`, and `.webmanifest` (26 files today) and deliberately skips KaTeX's `.woff`/`.ttf` fallbacks: they are ~876 KB of the 1.03 MB font payload and no browser that can run this React 19 bundle will ever ask for them. The cache name carries a **content hash** of everything precached, never a timestamp — a rebuild that changed nothing must produce a byte-identical `sw.js`, or every `cargo build` would hand users a "new version" prompt for an identical bundle. Two build-time invariants guard the silent-failure modes: a placeholder that has nothing to substitute, and a precache list missing the shell / a script / a stylesheet, both fail the build.

Runtime strategy:

- **Documents** are network-first with a 4 s timeout, falling back to the precached `/index.html`. The gateway is usually a local process and the bundle it serves is the source of truth; the cache is the offline floor, not the fast path. The timeout bounds the offline-but-not-refused case (captive portal, dropped VPN) that would otherwise hang.
- **Sub-resources** are cache-first. `/assets/*` is content-hashed so a hit can never be stale, and everything else rotates with the cache name on the next activation.
- **Google Fonts** get their own cache that is *not* build-scoped — the same bytes across deploys, and re-downloading ~300 KB on every gateway upgrade is waste. The stylesheet `<link>` carries `crossorigin="anonymous"` for this: without it the response is opaque, and an opaque error would pin itself in the font cache forever with no way to tell it from a success. (Self-hosting Inter / Space Mono the way KaTeX's faces already are would remove the third party entirely; not done yet.)
- `/v1/*`, `/healthz`, `/readyz` are never touched. A cached, bearer-scoped answer would be a stale lie.

**Updates** never happen under a live page. The worker does not `skipWaiting()` on install, because a dashboard tab is usually mid-something — a streaming turn, a half-typed message. Instead `src/pwa/registerSW.ts` notices the waiting worker and `PwaUpdateBanner` offers a reload (bottom-**right**: the chat composer owns the bottom of the reading band). Two rules in `src/pwa/updates.ts` are the ones that break silently, and they carry the unit tests: a worker reaching `installed` is only an *update* if the page already had a controller (otherwise every first-time visitor is told the app is out of date), and `controllerchange` may only reload when the user asked for it (it also fires when a first worker `clients.claim()`s the page — reloading there is a flash on first visit, and a loop if anything re-registers). A long-lived tab re-checks hourly, and on tab focus at most every 5 minutes.

**The gateway contract** (`crates/gateway/src/api/webui.rs`): any request whose last path segment carries an extension gets a 404, never the SPA fallback. That already protected a stale `<script src>` from being served HTML; with the PWA it also stops a `BAYBO_SKIP_WEBUI=1` build from answering `/sw.js` and `/manifest.webmanifest` with a page instead of admitting they aren't there. `/sw.js` rides the same `no-cache` branch as `index.html` and must: a cached worker script is a gateway upgrade the browser never notices. `mime_for` maps `webmanifest` to `application/manifest+json`, which Chrome and Firefox both require.

**Operational gotchas:**

- **Service workers need a secure context.** `http://127.0.0.1:8888` qualifies; `http://192.168.1.5:8888` does not, and neither the worker nor the install prompt appears there. The only signal is one `console.info` from `registerSW.ts` — nothing surfaces in the UI, by choice. Fixes, in order of effort: `ssh -L 8888:localhost:8888 <you>@<host>` and open the tunnelled `http://localhost:8888` (note the admin token is stored per origin, so the tunnelled URL asks for it again), or put the gateway behind an HTTPS reverse proxy (pass `Upgrade` through or the chat WebSocket dies).
  `pwa/availability.ts` decides this in one place. Note the check order there: `ServiceWorkerContainer` is `[SecureContext]`, so an insecure origin has **no `navigator.serviceWorker` at all** — testing for the API first (as this did at first) reports "unsupported browser", a dead end, for what is really a fixable URL problem, and the `console.info` above never printed.
- **There is no worker in `vite dev`** — `registerServiceWorker` returns early outside a production bundle, so HMR never fights a cache. To exercise it: `pnpm --filter baybo-web build && pnpm --filter baybo-web preview`, or build the gateway and open it on localhost.
- **Install button.** `beforeinstallprompt` is Chromium-only; the rail shows an install icon when it fires and hides it again on `appinstalled`. Safari never fires it — installs go through Share → Add to Home Screen, and `canInstall` staying false is the normal state on iOS.
- **Not** Web Push. The `web_push.*` records in the secret vault are the iOS app's *direct-mode APNs binding* (`crates/gateway/src/push/web.rs`), not W3C Web Push — there is no VAPID key, no `pushManager.subscribe`, and the worker has no `push` handler.
- **No `shortcuts` and no `screenshots`, deliberately.** The jump-list entries (right-click the installed icon → Chat / Traces / Cron) were built and then cut — the dashboard's own rail is one click away from every one of them, so the manifest was carrying three routes and three 96×96 icons to save nothing.
- **On `screenshots` specifically:** Chrome therefore uses its plain install dialog instead of the richer one, and DevTools → Application → Manifest says so with two warnings ("won't be available on desktop / on mobile"). They are expected; the images were built and then dropped. Anything that fills that field has to be captured against **mock data only** (`src/api/mock.ts`, reachable in dev via `?mock=true` with `/v1` proxied to a closed port) — these ship in the repo, ride into the gateway binary, and the browser shows them at install time, so a shot of a live instance would publish the owner's session titles and chat content.
- The dashboard is laid out desktop-first (`md:` appears five times in the whole app), so an installed phone window opens and works but is not responsive.

## Trace viewer polling

The per-session trace page (`app/web/src/pages/TraceSessionPage.tsx`) refreshes on a **two-tier, visibility-gated cadence** that stops entirely once nothing is live. There is **one** interval, not two: a tick bumps `refreshKey`, which refetches the overview and cascades into the step trees of every live turn. Its period is `POLL_ACTIVE_MS` (2s) when work is actually in flight — the selected turn is non-terminal, its tree holds a pending span, or **any nested subagent has a running turn** — and `POLL_TERMINAL_MS` (10s) when the only live thing is some other turn in the session. Both are skipped while the tab is hidden (`document.visibilityState !== 'visible'`), and when nothing is live no interval runs at all.

The live-subagent clause matters for external agents: the parent's `spawn_subagent` span sits pending for the whole child run, and once the parent turn goes terminal nothing else would hold the page on the fast tier — so a streaming child would fall to 10s, or stop refreshing entirely. `liveSessions` (`components/trace/traceForest.ts`) counts only `pending`/`in_progress`, deliberately **not** `stuck`: a stuck subagent is not producing anything new and must not pin the fast tier forever.

### Row order, and jumping to an id

Step and span rows carry their position in **storage order** — `#3` on a step,
`#3.2` on its second span — read off the arrays the API returns, which the
backend orders by `started_at` (`ORDER BY started_at` on both the step and the
span query). It is the index in the returned array, not a running count of
rendered rows, so an active filter never renumbers what survives it.

The tree's filter box doubles as a **jump box**, two ways:

- **By id** — step and span ids are part of every row's search projection, so
  pasting one (a 26-char ULID, either case) selects that node, scrolls to it,
  and clears the filter. The filter is the entry point on purpose: it already
  eager-loads every turn's tree, which is what lets an id resolve anywhere in
  the session rather than only in the turn that happens to be open.
- **By marker** — `#3` or `#3.2`, the position the tree prints on its own rows,
  resolved within the turn on screen (which is what those numbers are scoped
  to). The `#` is required so a bare `3` stays a text search: someone typing a
  number is looking for content, not navigating. `#0` is refused rather than
  silently resolving to the last row via a negative index.

`resolveJumpTarget` / `resolveOrdinalTarget`
(`components/trace/traceTreeModel.ts`) do the lookups; anything neither matches
stays an ordinary filter.

### The tool set behind an LLM call

An `LlmCall` span's detail panel gains a **Tools** tab listing every tool the
model was offered on that call, with its description and JSON schema. The span
stores only `{ hash, count }`; `GET /v1/traces/tool-sets/{hash}` returns the
definitions and the page caches them by hash. That cache is deliberately **not**
cleared on a session switch — the hash is the digest of the body, so the same
hash is the same set by construction. The fetch is lazy (it fires when the tab is
opened), which keeps a page visit at one request and a reader who never opens the
tab at zero. See [`modules/trace.md`](modules/trace.md) for why the definitions
are not inlined into the span.

### The context matrix

An `LlmCall` span's detail panel also carries a **Context** tab: a grid of
cells, one per fixed slice of tokens, coloured by which part of the assembled
context it belongs to — system prompt, tool definitions, skills, recalled
memory, each message kind, tool results, attachments. When the span's model is
still served by a configured client the grid covers that model's whole context
window, so the unfilled cells are the headroom the call had left; when it is
not (the trace outlives the config) the grid is the input alone rather than an
invented window.

`GET /v1/traces/{session_id}/spans/{span_id}/context` computes it, because the
split needs a tokenizer *and* the ordinal-referenced input slice the client
holds only as a pointer. It is the most expensive read on this page — it
resolves both of the references the per-turn tree leaves alone — so it is
fetched only when the tab is opened, cached per span, and deliberately **not**
refreshed on the poll tick: a live span has no usage to report yet, and
re-tokenizing a growing transcript every two seconds would make the most
expensive read the most frequent one.

**The total is exact and the split is not.** The provider bills one number for
the whole prompt; the per-part figures come from `tiktoken`, which is a ~10%
approximation off OpenAI models. `buildContextGrid`
(`components/trace/contextGrid.ts`) therefore uses the segments only for
proportions and applies them to the recorded `input_tokens`, so the headline
agrees with the Metadata tab; every per-part number is prefixed `≈`, and a
drift above 15% is stated outright. Cells are allocated by largest remainder so
they sum to exactly the grid, with a floor of one cell for any non-zero part —
rounding a real 0.4% contributor away is how a context view quietly stops
mentioning the thing someone opened it to find.

**One scaling site, one denominator.** The panel shows the same numbers twice —
a legend by part, and a "largest pieces" list by individual segment — so both
read from `ContextGrid.segments`, which `buildContextGrid` scales once. Scaling
in two places is exactly how the legend and the list first came to print two
different figures for one tool set. For the same reason every percentage is a
share of **what was sent**, never of the window: a part that is 33% of the input
and 5% of the window has to pick one, or the two lists cannot be read against
each other. The window appears only as the headline's "% full" and as the free
cells, and the free legend row deliberately prints no percentage at all.

Repeated labels carry their position (`#12 read_file result`): five `read_file`
calls produce five identically-named segments, and without the index there is no
way to tell which one is the 40k one. **That position is also the way in** —
clicking it opens the piece it names: a message scrolls into view in the I/O
tab (unfolding the "earlier messages" block and expanding the card if the target
is inside them, or the jump lands on something not mounted), and the tool set,
which is not a message and has no position of its own, opens the Tools tab
instead.

**One explanation surface, no popups.** Hovering a grid cell or a legend row
names the part and what it holds in a **reserved line under the legend**, and
dims every cell that is not that part so the reader sees where it sits in the
matrix. Nothing in this area carries a native `title`: the panel is a scroll
container, so an absolutely-positioned bubble clips at its edges; the `title`
delay is long enough to read as nothing happening; and while both existed they
fired together, the line immediately and the popup a second later with the same
words. The line has a fixed min-height so nothing reflows as the pointer moves.
"Agent-injected" is the part that most needs the explanation — a catch-all
holding an invoked skill's body, a subagent task prompt or its finished
notification, and compaction instructions.

The one `title` left in the panel is on the `#N` jump button, because it names
an **action** rather than a thing on screen, and a button that does not say
where it goes is worse than a tooltip.

The legend carries **both** denominators when a window is known: share of the
input (what is eating the prompt) and share of the window (what it costs in
room). Free has no share of the input — it is not part of it — so that cell is a
dash and its real number sits under the column that measures it.

### Subagent lineage

`GET /v1/traces/{session_id}/lineage` returns every subagent session descended from this one, flattened, each row carrying its attach point (`parent_span_id` — the parent's `spawn_subagent` tool-call span), its backend (`external_agent`, absent for in-process children), and its turn summaries. It refreshes on the same tick as the overview, so a subagent spawned mid-turn appears without a manual reload.

The client indexes it with `buildForest` and nests each child **in place** under the span that spawned it: expanding a child fetches its own overview (transcript + turns) and, for a baybo-backed child, its per-turn step trees — an external child has no step tree to fetch, so those round trips are skipped entirely. Navigating to the child's own page is still there as a secondary action. A collapsed subagent row carries a roll-up failure badge computed across the whole subtree from turn statuses, which the lineage response supplies up front — that is what lets "something failed in there" show before anything is expanded.

Both walks are bounded and de-duplicated: `QueryApi::load_lineage_overview` keeps a visited set (a depth cap alone would still walk a cycle repeatedly) and caps at `MAX_LINEAGE_DEPTH` / `MAX_LINEAGE_SESSIONS`, warning when it truncates; the render path carries its own `MAX_RENDER_DEPTH` backstop so malformed lineage degrades to a truncation notice instead of a blown stack.

`/lineage` is the expensive call in the family — the server spends roughly four store round trips per descendant — so it is throttled to the **slow** tier even while the overview polls fast. The set of subagents changes when one is spawned, not when a token arrives.

**Known gap:** the trace-overview strip (`TraceOverviewBar`) and the turn column (`TurnAnchors`) still summarise only the *viewed* session's turns. A subagent's steps, spans, failures, and tool calls are drawn in the tree but do not appear in the strip's counts or its clickable minimap, so on a trace with subagents the strip under-reports. This predates inline nesting (subagents were not drawn at all before) and is not a regression, but it is now visibly inconsistent with the tree below it.

The overview poll is **incremental**. `GET /v1/traces/{session_id}?since_ordinal=N` returns only `session_messages` rows with `ordinal > N` (each still carrying its `superseded_by` marker), always the full (tiny) `turns` array, and a top-level `supersede_watermark` — the session's highest `superseded_by`, which advances only when a compaction re-marks rows. Each poll passes `since_ordinal` = the highest ordinal already held, then **appends** the strictly-newer delta rows (no dedup) and replaces `turns`; a freshly-opened session or cold start omits the param and takes the full page. When the response's `supersede_watermark` differs from the cached one, a compaction re-marked rows the client may hold, so the cached prefix is stale — it is dropped and reloaded in full once (no param). The append allocates new `session_messages` **and** overview object references so every messageLog-derived memo (persisted-span-input hydration, the interjection-span index) recomputes.

Deep links carry the selection in the query string: `?turn`, `?step`, `?span`, `?msg` (a transcript row, as `ordinal:blockIndex`), `?child` (a subagent boundary), and `?tab`. The `turn` param is validated against the turns the page knows about — the session's own **and** every nested subagent's, since turn ids are globally unique — and falls back to the oldest turn of the session being viewed when it names nothing. Selecting any one kind clears the others, so the URL never disagrees with the highlighted row.

This watermark protocol is deliberately **not** the chat sync protocol ([`docs/sync-protocol.md`](sync-protocol.md)): the trace viewer is a read-only admin poller over the *full* transcript (superseded rows included) with no outbox, no WS subscription, and no per-device cursor durability — sync-v2's rebase/gap machinery would be overkill, and the one staleness event that matters here (compaction re-marking held rows) is exactly what the watermark encodes. A new *chat-facing* surface should reach for sync-v2, not copy this.
