---
name: deck
version: 0.3.0
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
≤32 files, ≤256 KB each, ≤2 MB total; no symlinks. Most cards need no `src/`
— write `card.html` directly.

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
  one emit per window (clamped to ≥10s); excess emits coalesce to latest.

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
  addresses are blocked (SSRF floor); redirects are not followed.
- `await ctx.exec(cmd)` → `{code, stdout, stderr}`. Runs `/bin/sh -c` on
  the host with the inherited environment (installed CLIs and credential
  dirs resolve, network available), 10s cap, 256KB output cap. Use for
  host state and CLIs: `df -k`, `uptime`, `ps`, `git -C <dir> log`,
  `codex …`, etc.
- `ctx.emit(json)` — push a fresh snapshot to the phone (rate-policed).
- `ctx.log(msg)` — diagnostic logging (console.log also routes here).

Per-op calls have a 10s budget; a timeout fails the call, not the
process. Repeated crashes/timeouts quarantine the card (visible error
face + Re-enable), so keep ops fast and handle upstream errors — return
`{error: "..."}`-style JSON rather than throwing when the upstream is
merely down.

### card.html — the frontend

A fragment rendered inside a sandboxed iframe (opaque origin, CSP: no
network, no external resources, inline `<script>`/`<style>` + `data:`
images only). The shell injects a `deck` global before your code runs:

- `deck.onData(fn)` — called with the latest snapshot immediately (the
  cached one) and again on every live push. Render from here.
- `await deck.call(op, params)` — invoke one of your ops on demand
  (user taps, drill-downs). Same validation as any call.
- `deck.size` — current render size: `"small"` | `"wide"` | `"large"` |
  `"max"` (while maximized).
- `deck.onSizeChange(fn)` — called with the current size immediately and
  again whenever it changes (resize, or maximize/restore). This is how you
  adapt: show more rows at `large`, the full view at `max`, less at `small`.

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
`deck.size === "max"`. The shell then shows a ⛶ button in the top-right; a
tap expands the card to fill the screen (the card never reloads — same
document, `deck.size` flips to `"max"`), and a ✕ in the same corner
restores it. The maximized layout **may scroll** (it's the whole screen, not
a tile) and is where a card earns its detail: a full history, a chart, a
table. Two rules:

- **Keep the top-right clear** — the ✕ lives there (~34×34pt below the
  status bar); don't put a tappable control or key number under it.
- **Keep the bottom clear** — the tab bar floats over the bottom of the
  maximized card. End your content with
  `padding-bottom: calc(env(safe-area-inset-bottom) + 64px)` so nothing
  important hides behind it.

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
