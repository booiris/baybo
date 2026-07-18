---
name: deck-card
version: 0.2.0
description: "Author and install a live card on the user's Deck (the dashboard tab of agent-written cards) — e.g. a Claude/Codex quota monitor, a machine-status board, an API watcher. Invoked explicitly by the user typing /card <request>; never auto-selected. Covers the bundle contract, the service/card SDK surface, worked examples, and the install flow via DeckCardCreate/DeckCardUpdate."
command: card
user-invocable: true
disable-model-invocation: true
channels:
  - owner
allowed-tools:
  - Write
  - Edit
  - Read
  - Bash
  - DeckCardCreate
  - DeckCardUpdate
---

# Authoring a deck card

The user invoked `/card` — the text after the command is their request.
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
```

### manifest.json

```json
{
  "title": "Claude quota",
  "size": "wide",
  "refresh": { "op": "refresh", "min_emit_interval_secs": 300 }
}
```

- `size`: `small` (1×1) | `wide` (2×1) | `large` (2×2). Title and size
  are install-time defaults; the user's layout owns them afterwards.
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
- `deck.size` — current size class string.

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
  less — a clipped half-line reads as a bug.

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

## Install flow

1. Write the four files into a scratch directory.
2. `DeckCardCreate(path: "<absolute staging dir>")`.
3. If the gate fails, read the error (it includes your service's
   stderr), edit the files, and call `DeckCardCreate` again.
4. To change an existing card: stage the new bundle and call
   `DeckCardUpdate(card_id, path)`. The user's title/size/layout are
   preserved; only your code and contract change.

Tell the user the card is on their Deck once the install succeeds. Do
not fabricate data sources: if the user's request needs an API you can't
reach or credentials that don't exist, say so and build the closest
honest card instead.
