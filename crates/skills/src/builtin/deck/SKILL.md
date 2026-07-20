---
name: deck
version: 0.3.1
description: "Author and install a live card on the user's Deck (the dashboard tab of agent-written cards) — e.g. a Claude/Codex quota monitor, a machine-status board, an API watcher. Invoked explicitly by the user typing /deck <request>; never auto-selected. Covers the bundle contract, per-size adaptation and the optional maximized layout, the service and card SDK surface, worked examples, and the install flow via DeckCardCreate/DeckCardUpdate."
command: deck
user-invocable: true
disable-model-invocation: true
channels:
  - owner
allowed-tools:
  - Write
  - Edit
  - Read
  - Bash
  - DeckCardList
  - DeckCardGet
  - DeckCardCreate
  - DeckCardUpdate
---

# Authoring a deck card

The user invoked `/deck` — the text after the command is their request.
Build the card they asked for. If the request is empty, ask what the
card should show before writing anything.

A deck card is a self-contained bundle of exactly four plain files. You
write them in a scratch directory with your normal file tools, then
install with `DeckCardCreate(path)`. Install runs a **dry-run gate**:
your `service.js` is booted for real on the host and the refresh op
is invoked once — failures (including the service's stderr) come back in
the tool result so you can fix and retry in the same turn. On success
the card appears on the user's deck already showing data.

## The bundle

```
<staging-dir>/
  manifest.json
  openapi.json
  service.js     backend — runs on the gateway host (use ctx.fetch / ctx.exec)
  card.html      frontend — runs on the phone in a sandboxed iframe, NO network
  src/           OPTIONAL — pre-build sources kept with the bundle (not run)
```

**`src/` and `bun build` (optional).** `card.html` must be a single
self-contained file, but for a complex card you may author it as TypeScript /
multiple modules under `src/` and build the one `card.html` with `bun build`
on the host (it's installed), e.g.
`bun build src/card.ts --outfile card.html` (no `--minify` — keep it
readable/diffable). Put the sources under `src/` and they ride along with the
bundle: the gateway never runs or reads them, but `DeckCardGet` hands them
back so a later edit works from the real inputs, not the built output. Caps:
≤30 MB per file, ≤60 MB total; no symlinks (file count is unbounded). Most
cards need no `src/` — write `card.html` directly.

### manifest.json

```json
{
  "title": "Claude quota",
  "size": "wide",
  "sizes": ["wide", "large"],
  "maximize": true,
  "refresh": { "op": "refresh", "min_emit_interval_secs": 300 }
}
```

- `size`: `small` (1×1) | `wide` (2×1) | `large` (2×2). The install-time
  default; the user's layout owns it afterwards. Must be a member of `sizes`.
- `sizes` (optional): every grid size your `card.html` actually adapts to.
  The user's ⤢ resize cycle is confined to this set, so **only list a size
  you have laid out** — a size you declare but don't handle renders clipped.
  Omit it for a single-size card (equivalent to `["<size>"]`); the ⤢ control
  hides. Pick sizes that suit the data: one hero number fits `small`; a hero
  plus a few rows wants `wide`; two rows of that or a small chart wants
  `large`.
- `maximize` (optional, default false): declare `true` only if you provide a
  full-screen `"max"` layout (see below). It adds a ⛶ button in the tile's
  **top-right** — so if you set it, keep the top-right ~34×34pt of every
  layout clear of important content.
- `refresh.op` must exist in `openapi.json`; the gate calls it once with
  `refresh.params` (optional object).
- `min_emit_interval_secs`: your emit floor. The gateway accepts at most
  one emit per window (clamped to ≥1s); excess emits coalesce to latest.

### openapi.json — the op contract

Every call that crosses the gateway is validated against this document
**before** your code sees it: unknown ops 404, unknown/mistyped/missing
params 400. Declare each op as `paths./<name>.get` (or `post`) with
typed parameters. Op names: `[a-z][a-z0-9_]*`.

Each parameter's `schema` is **standard JSON Schema** — scalars, enums,
and also `array` and `object` params with nested typing and constraints
(`items`, `properties`, `minimum`, `maxItems`, …) all validate. Keep it
inline: `$ref` is rejected.

**Every op MUST declare `x-baybo-retryable` (boolean) — install fails
without it.** It answers one question: if this op ran but its response
was lost in transit, is silently running it AGAIN harmless? `true` only
for pure reads/recomputes (fetch-and-format, stat collection); `false`
for anything with side effects (a POST to an external API, a mutating
command). A `false` op that dies mid-transit surfaces the error to the
card instead of being replayed.

```json
{
  "openapi": "3.1.0",
  "paths": {
    "/refresh": { "get": { "x-baybo-retryable": true } },
    "/history": { "get": { "x-baybo-retryable": true, "parameters": [
      { "name": "days", "required": true, "schema": { "type": "integer" } }
    ] } }
  }
}
```

### service.js — the backend

Pure logic; the runtime preamble owns all protocol plumbing. Export an
`ops` map (each op: `async (params, ctx) => json`) and optionally
`start(ctx)` for your own timer. **The refresh op must return the card's
snapshot JSON (not null).**

`ctx` surface (universal — no declaration or configuration):

- `await ctx.fetch(url, {method, headers, body})` →
  `{status, headers, body, json()}`. Host-mediated (the Rust parent makes
  the request): **always use this for HTTP** — it is the only path that
  reveals vault secrets. Put a secret's `[{REDACTED_SECRET_…}]`
  placeholder in a header or the URL and it is substituted only at egress
  in the parent; never ask for or embed a raw secret. Loopback/LAN
  addresses are blocked (SSRF floor); redirects are not followed. The whole
  response buffers in the gateway, so fetch API/JSON and small assets — not
  multi-GB downloads or endless streams.
- `await ctx.exec(cmd)` → `{code, stdout, stderr}`. Runs `/bin/sh -c` on
  the host with the inherited environment (installed CLIs and credential
  dirs resolve, network available), 30s cap, 4MB output cap. Use for
  host state and CLIs: `df -k`, `uptime`, `ps`, `git -C <dir> log`,
  `codex …`, etc.
- `ctx.emit(json)` — push a fresh snapshot to the phone (rate-policed).
- `ctx.log(msg)` — diagnostic logging (console.log also routes here).

Blobs (files/images). Bytes stay out of your service — you pass around a
small `ref` (`{blobId, contentType, size}`) and let the phone fetch/display
the bytes. Put a ref into the snapshot you `emit`; the card renders it with
`deck.blobUrl` (below).

- `await ctx.fetchBlob(url, {method, headers, body})` → `ref`. Like
  `ctx.fetch`, but streams the response straight into storage instead of
  returning the body — use it for an image/file you want to DISPLAY or hand
  back, of any size. 2xx only, bounded redirects, same secret-placeholder
  reveal as `ctx.fetch`.
- `await ctx.blobPutFile(path, contentType)` → `ref`. Store a file a
  `ctx.exec` produced (relative `path` resolves against the exec working dir).
  This is the path for content your service *generates*: write it to disk in
  an `exec`, then hand the file over. (For a small image you only need to
  DISPLAY, not save, just emit a `data:` URI in your snapshot — no blob needed.)

**Reuse a `blobId` for unchanged content — do NOT re-fetch every tick.** A
`blobId` is stable only while you keep the same one: fetching or putting the
SAME bytes AGAIN mints a NEW id (each is a fresh read capability). A
refresh-loop card that re-`fetchBlob`s the same image every tick emits a
different id each time, and the phone visibly re-paints the picture on every
refresh — it does not re-download (the bytes are cached), but the image blinks.
So cache the id in your `start()` closure: fetch/put once, keep the `blobId`,
re-emit the SAME id, and only fetch again when the content actually changes
(e.g. the upstream `ETag`/`Last-Modified` moved, or a new day's chart).

Per-op calls have a 30s budget; a timeout fails the call, not the
process. Repeated crashes/timeouts quarantine the card (visible error
face + Re-enable), so keep ops fast and handle upstream errors — return
`{error: "..."}`-style JSON rather than throwing when the upstream is
merely down.

### Driving a terminal CLI through tmux — use the injected private socket

Some cards report on a CLI that only speaks through a real terminal (an
interactive quota TUI, `claude`, `codex`). `ctx.exec` gives no PTY and caps
each call at 30s, so run the CLI in a **detached tmux session** that outlives
the op: create it once, then `capture-pane` (or `send-keys`) on later refreshes.

Never invoke bare `tmux` — that binds the **user's own default socket**
(`/tmp/tmux-<uid>/default`), so your card's session pollutes their `tmux ls`
and a `/tmp` wipe kills it. Every `ctx.exec` is handed **`$BAYBO_DECK_TMUX_DIR`**
(`<workspace>/deck/tmux-socks/<card-id>` — private to your card) for exactly
this — pin your socket there with `-S`:

```js
refresh: async (_p, ctx) => {
  const { stdout } = await ctx.exec(`
    S="$BAYBO_DECK_TMUX_DIR/quota.sock"        # injected per-card dir, never /tmp
    mkdir -p "$(dirname "$S")"
    tmux -S "$S" has-session -t q 2>/dev/null ||
      tmux -S "$S" new-session -d -s q -x 140 -y 48 'exec claude --safe-mode'
    tmux -S "$S" capture-pane -p -t q
  `);
  return parseQuota(stdout);
},
```

- **`-S "$BAYBO_DECK_TMUX_DIR/…"` is the whole isolation.** The socket lives
  under `<workspace>/deck`, not `/tmp`, so a `/tmp` wipe can't kill it and the
  user's `tmux ls` never sees it. It hides the session from their *list*, not
  from a same-UID `tmux -S <path> attach` — that's fine; the goal is to keep
  deck plumbing out of their view, not to sandbox it. (`mkdir -p` it first —
  the runtime exports the path but doesn't create the dir.)
- **The session is shared host state, not per-op.** Guard creation with
  `has-session` so each refresh reuses the running session instead of spawning
  a fresh server every tick; `kill-session` it once the card's job is done.
  Purging the card kills whatever servers still sit behind `*.sock` files in
  its dir — but that's the last-resort sweep, not a reason to leave one
  running.

### card.html — the frontend

A fragment rendered inside a sandboxed iframe (opaque origin, CSP: no
network except displaying stored blobs via `deck.blobUrl`, no external
resources, inline `<script>`/`<style>` + `data:`/blob images). The shell
injects a `deck` global before your code runs:

- `deck.onData(fn)` — called with the latest snapshot immediately (the
  cached one) and again on every live push. Render from here.
- `await deck.call(op, params)` — invoke one of your ops on demand
  (user taps, drill-downs). Same validation as any call.
- `deck.size` — current render size: `"small"` | `"wide"` | `"large"` |
  `"max"` (while maximized).
- `deck.onSizeChange(fn)` — called with the current size immediately and
  again whenever it changes (resize, or maximize/restore). This is how you
  adapt: show more rows at `large`, the full view at `max`, less at `small`.
- `deck.blobUrl(ref)` → a URL string for an `<img src>`. `ref` is a blob ref
  from your snapshot (`{blobId, contentType}`) or a bare `blobId`. Point an
  `<img>` at it — the shell serves the bytes (cache-first, so it works
  offline once fetched). The same ref always yields the same URL, so the
  image only re-paints when you emit a NEW ref — see the reuse rule above.
  Handle `<img onerror>` if a blob might be unavailable.
- `await deck.pickBlob({accept})` → `{blobId, contentType, size, name}` — ask
  the user to pick a photo (the native picker). Resolves once the pick
  uploads; pass the `blobId` into `deck.call(op, {blobId})` for your service
  to consume. Rejects on cancel, or `busy` if a pick is already open (one at
  a time). Every call settles exactly once.
- `deck.shareBlob(ref, {filename})` — offer a blob to the system share sheet
  (save to Photos/Files, AirDrop). Fire-and-forget.

**Render snapshot data as text, not markup.** The iframe blocks network, but
the data you paint (a `ctx.fetch` body, an emitted snapshot) is untrusted — set
`el.textContent`, never `el.innerHTML`, or injected `<img onerror=…>` markup
would run inside the card. Build DOM with `createElement` + `textContent` (the
worked examples do).

### Adapting to size — one document, `deck.onSizeChange`

Your card is **one** `card.html`. Don't guess a size at load; drive layout
off `deck.onSizeChange`. The idiomatic pattern is to reflect the size onto a
root attribute and let CSS do the rest, so a resize never reloads the card:

```js
deck.onSizeChange((s) => { document.documentElement.dataset.size = s; });
```
```css
.detail { display: none; }              /* hidden at small/wide */
[data-size="large"] .detail,
[data-size="max"] .detail { display: block; }
[data-size="small"] .hero { font-size: 20px; }
```

Only declare a `sizes` entry you've actually laid out this way.

### Card design — a base stylesheet is injected, use it

The app is **soft monochrome line minimalism**: near-black ink on
paper-white, thin hairlines, generous whitespace, zero decoration. To
keep every card in that language, the shell injects a **base
stylesheet before your fragment** — body font/color defaults plus:

- Tokens: `--ink` `--muted` `--line` `--ok` `--bad`.
- Classes: `.card` (flex column filling the tile, padded),
  `.label` (small muted caption row), `.hero` (the one big number),
  `.row` (space-between line), `.divider`, `.foot` (bottom-pinned muted
  footer), `.dot` / `.dot.bad` (status dot), `.bar > i` (thin progress,
  set the `i` width in %).

**Prefer the classes; write only layout CSS of your own.** Everything
is overridable — your `<style>` comes after the base in the cascade,
and the palette is custom properties — but overriding the look is the
exception, not the norm.

Hard rules:

- **No gradients, no box-shadows, no emoji, no icon fonts.** Flat
  paper. Color is for STATUS only (`--ok`/`--bad` dots or a bar fill) —
  never colored backgrounds, pills, or per-metric accents.
- **Don't draw your own card surface** — no outer border, radius, or
  page background. The shell's tile IS the surface; separate regions
  with `.divider` or whitespace, not nested boxes.
- **One small heading, yours** (a `.label` is usually right). The shell
  shows the manifest title only while the user edits the layout — in
  normal view your fragment owns the heading. Never a big banner.
- **Design to the declared size — tiles clip, they never scroll.** The
  iframe viewport is exactly the tile interior (iPhone-class, pt):
  `small` ≈ 178×148, `wide` ≈ 368×148, `large` ≈ 368×310. `.card` +
  `.foot` compose against that; 1 hero + 2–3 secondary metrics fit a
  `wide`, a `large` holds two rows of that. If it doesn't fit, show
  less — a clipped half-line reads as a bug. Each size you list in `sizes`
  is a layout you own — test that it fills its tile without clipping.

### Maximizing (optional) — the `"max"` size

Set `"maximize": true` only if you build a full-screen layout for
`deck.size === "max"`. On a maximize-capable card the shell shows a ⛶ button
in the tile's top-right; a tap expands the card (the card never reloads — same
document, `deck.size` flips to `"max"`). The user restores it with the ✕ in
the app's header or by **swiping in from the left edge** — both are handled for
you, so a `max` layout needs no exit control of its own. The maximized layout **may scroll**
(it's the whole screen, not a tile) and is where a card earns its detail: a
full history, a chart, a table. Rules:

- **Leave the tile's top-right for the ⛶ in the grid sizes.** In `small` /
  `wide` / `large`, the ⛶ sits in the tile's top-right (~40pt) — keep your own
  tappable controls / key numbers out of that corner (e.g. a top row that
  right-aligns a control: `padding-right: 42px`). At `max` the corner is
  yours again (the ✕ is up in the app header), so a right-aligned control can
  go to the far right.
- **Clear the header at max.** The app's header stays visible above the
  maximized card, so start your `max` content below it:
  `padding-top: calc(env(safe-area-inset-top) + 54px)`.
- **Clear the tab bar at max.** It floats over the bottom. End your content
  with `padding-bottom: calc(env(safe-area-inset-bottom) + 64px)`.

If you don't provide a `"max"` layout, leave `maximize` out — a ⛶ that
expands to the same tile layout stretched full-screen looks broken.

Self-contained: no frameworks, everything inline, and render something
sensible for `{error: "..."}` snapshots (a muted line, not a red wall).

## Worked example 1 — API watcher (fetch-shaped)

```js
// service.js
const URL = "https://api.example.com/v1/usage";
export const ops = {
  refresh: async (_p, ctx) => {
    const r = await ctx.fetch(URL, {
      headers: { "x-api-key": "[{REDACTED_SECRET_…}]" },
    });
    if (r.status !== 200) return { error: `upstream ${r.status}` };
    const d = r.json();
    return { used: d.used, limit: d.limit, at: Date.now() };
  },
};
export function start(ctx) {
  const tick = async () => ctx.emit(await ops.refresh({}, ctx));
  setInterval(tick, 300_000);
}
```

## Worked example 2 — machine status (exec-shaped, no fetch)

```js
// service.js
export const ops = {
  refresh: async (_p, ctx) => {
    const disk = await ctx.exec("df -k /");
    const load = await ctx.exec("uptime");
    return { disk: disk.stdout, load: load.stdout.trim(), at: Date.now() };
  },
};
export function start(ctx) {
  const tick = async () => ctx.emit(await ops.refresh({}, ctx));
  setInterval(tick, 60_000);
}
```

```html
<!-- card.html — the injected base does the design; the card is just
     structure: label + hero + bottom-pinned footer. The one custom
     rule shows the override path. -->
<style>
  .hero { margin-top: 6px; }
</style>
<div class="card">
  <div class="label"><span class="dot" id="dot"></span>负载</div>
  <div class="hero" id="v">–</div>
  <div class="foot" id="d"></div>
</div>
<script>
  deck.onData((s) => {
    const bad = !!s.error;
    document.getElementById("dot").className = "dot" + (bad ? " bad" : "");
    document.getElementById("v").textContent = s.error ?? s.load ?? "–";
    document.getElementById("d").textContent =
      s.at ? new Date(s.at).toLocaleTimeString() + " 更新" : "";
  });
</script>
```

## Worked example 3 — cover image (blob-shaped)

Fetch a picture ONCE and reuse its `blobId` until the source changes, so the
card doesn't blink every tick (the reuse rule above).

```js
// service.js — a daily featured image.
const META = "https://api.example.com/v1/featured";
let last = { url: null, cover: null }; // remembered across ticks
export const ops = {
  refresh: async (_p, ctx) => {
    const r = await ctx.fetch(META);
    if (r.status !== 200) return { error: `upstream ${r.status}` };
    const d = r.json();
    // Re-fetch the image ONLY when the source changed — otherwise keep the
    // blobId we already have (a fresh fetchBlob would mint a new id and blink
    // the picture on every refresh).
    if (d.imageUrl !== last.url) {
      last = { url: d.imageUrl, cover: await ctx.fetchBlob(d.imageUrl) };
    }
    return { title: d.title, cover: last.cover, at: Date.now() };
  },
};
export function start(ctx) {
  const tick = async () => ctx.emit(await ops.refresh({}, ctx));
  setInterval(tick, 3_600_000);
}
```

```html
<!-- card.html — deck.blobUrl turns the ref into an <img src>; the same ref
     yields the same URL, so the image only repaints when a NEW cover lands. -->
<style>
  .cover {
    width: 100%;
    aspect-ratio: 16 / 9;
    object-fit: cover;
    border-radius: 6px;
  }
</style>
<div class="card">
  <img class="cover" id="img" alt="" />
  <div class="label" id="t">–</div>
</div>
<script>
  deck.onData((s) => {
    document.getElementById("t").textContent = s.error ?? s.title ?? "–";
    const img = document.getElementById("img");
    if (s.cover) img.src = deck.blobUrl(s.cover);
    img.onerror = () => deck.log("cover unavailable");
  });
</script>
```

## Install flow (new card)

1. Write the four files into a scratch directory.
2. `DeckCardCreate(path: "<absolute staging dir>")`.
3. If the gate fails, read the error (it includes your service's
   stderr), edit the files, and call `DeckCardCreate` again.

## Updating an existing card

The user's cards persist across conversations, so this works even in a
brand-new chat where you have no memory of the card. **Do not re-write a
card from scratch to change it** — you would lose whatever it already did.
Instead, edit its real source:

1. `DeckCardList()` — lists every live card (`card_id`, `title`, `size`,
   `sizes`, `maximize`, …). Match the user's description ("the quota card")
   to a `title` to get its `card_id`. If it's ambiguous, ask which one.
2. `DeckCardGet(card_id)` — returns that card's current source verbatim: the
   four bundle files plus any `src/` pre-build files (keyed by their `src/…`
   path). Write them all into a fresh scratch directory. If the card has a
   `src/`, edit the source and re-run `bun build` to regenerate `card.html`;
   otherwise edit `card.html` directly.
3. Make the surgical change the user asked for (edit the relevant file; leave
   the rest byte-for-byte).
4. `DeckCardUpdate(card_id, path)` — same dry-run gate as create; on
   success the service restarts on the new code. The user's title, size,
   and layout are owned by the card after install, so the manifest's
   values do **not** overwrite them.

Every install/update/purge is auto-committed to a git repo under the deck
root, so the operator can diff and roll back a card by hand — you don't
manage that, but it's why editing from the real source matters.

Tell the user the card is on their Deck once the install succeeds. Do
not fabricate data sources: if the user's request needs an API you can't
reach or credentials that don't exist, say so and build the closest
honest card instead.
