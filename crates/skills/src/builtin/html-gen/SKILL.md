---
name: html-gen
version: 0.1.0
description: "Create and publish an interactive HTML page in the iOS conversation, optionally loading a JavaScript library you publish alongside it. Use for visual explainers, diagrams, charts, dashboards, mini apps, simulations, rich reports, or any request that is better experienced as a rendered webpage than plain text."
channels:
  - owner
argument-hint: "[page request]"
allowed-tools:
  - Read
  - Write
  - Edit
  - Bash
  - PutBlob
---

# Generate an HTML preview

Create the page requested by the user. For `/html-gen`, treat the text
after the command as the request; if it is empty, ask what to build.

## Write the page

Write one complete HTML document to an absolute path in the workspace, with
all CSS and your own JavaScript inline, graphics as inline SVG or `data:`
images, and any library published as a blob (see "Bringing a JavaScript
library" below — usually you need none).

### The sandbox it runs in

The document is served to an `<iframe sandbox="allow-scripts">` — no
`allow-same-origin` — under exactly this response CSP:

```
default-src 'none'; script-src 'unsafe-inline' <app-origin>/html-lib/;
style-src 'unsafe-inline'; img-src data:; connect-src 'none'; frame-src 'none';
object-src 'none'; base-uri 'none'; form-action 'none'
```

`<app-origin>` is whatever origin the app serves the document from, and it
differs per platform — you never write it out, because the only thing you may
load from it is a `/html-lib/` bundle the app hands you by name.

**Treat anything not granted above as unavailable**, and never put a
control in the UI for something that cannot work — a button that does
nothing reads worse than no button at all. What that rules out:

- **No `'unsafe-eval'`.** `eval`, `new Function`, and string-argument
  `setTimeout` / `setInterval` are blocked. Parse and dispatch by hand.
- **No fonts.** There is no `font-src`, so `@font-face` fails even from a
  `data:` URI. Use system font stacks.
- **No dialogs.** `alert`, `confirm` and `prompt` are silent no-ops
  without `allow-modals`. Render messages into the page instead.
- **No media, workers, or `blob:` URLs.** `<audio>` / `<video>` load
  nothing even from `data:`, `Worker` is blocked, and
  `URL.createObjectURL` output is not a permitted source — export a
  canvas with `toDataURL()`.
- **No network at all.** `fetch`, `XMLHttpRequest`, `WebSocket`,
  `EventSource` and `sendBeacon` are dead. Apart from a `/html-lib/` script
  you published yourself, the page must carry every byte it needs.
- **No navigation or exit.** `window.open`, `target="_blank"`, external
  links, `<form>` submission and `<a download>` all do nothing.
- **No storage.** The origin is opaque: cookies, `localStorage`,
  `sessionStorage` and IndexedDB are unavailable. Hold state in memory.
- **No device APIs.** Permissions-Policy disables camera, microphone,
  geolocation, fullscreen and the rest.

### The frame it lives in

- **It resizes.** The page renders as a 420pt inline card that the reader
  can expand to full screen and back — the same iframe, never reloaded,
  so JS state survives the toggle. Lay out so it reflows live between the
  two; do not design for one size. The host toolbar already owns the
  expand and reload controls, so do not build your own (and
  `requestFullscreen` is disabled regardless).
- **Reload restarts it.** The toolbar's ↻ re-runs the document from
  scratch and in-memory state is lost, so the opening state has to be
  worth looking at on its own. Never require setup before the page shows
  anything.
- **The app is light-only.** The transcript pins `color-scheme: light`,
  but this document is a separate browsing context and inherits none of
  it. Set `color-scheme: light` yourself and write **no**
  `prefers-color-scheme: dark` rules, or the preview lands as a dark slab
  inside a light app.
- **Full screen reaches the bottom edge.** Host chrome covers only the
  top safe area. Give anything anchored to the bottom a fixed padding of
  its own rather than relying on `env(safe-area-inset-bottom)`.

Include a viewport meta tag, keep controls keyboard-reachable, and label
them so a screen reader can announce them.

### Bringing a JavaScript library

Nothing is provided for you. If a page genuinely needs a library, YOU obtain it
and publish it, exactly as you publish the page:

1. Download it to a file with a shell command — a specific version's minified
   **UMD/IIFE** bundle (an ES module will not load here) from the project's own
   distribution. Fetch it straight to disk and **never read it back**: these
   files are hundreds of kilobytes of minified source, and nothing about them
   is worth spending your context on.
2. `PutBlob` it with `mime_type: "text/javascript"`.
3. Reference the returned id as the script's source:

```html
<script src="/html-lib/sha256:…"
        onload="draw()" onerror="failed()"></script>
```

`/html-lib/<blob id>` is the ONLY source the CSP's `script-src` admits besides
your own inline code. A CDN URL will not load — there is no network — and the
refusal is silent from the reader's side unless you handle `onerror`.

**Reference it; do not paste it into the page.** Concatenating the library into
the document would run — inline script is allowed — which is exactly why the
mistake is easy to make and gives no error. But a library published on its own
is content-addressed, so it is stored once and downloaded to a device once, no
matter how many pages use it; a library pasted into each page is bytes the
reader pulls again every single time.

Three rules:

- **Weigh it first.** A library is hundreds of kilobytes the reader waits for
  and you have to be right about. Most pages want none: hand-written inline SVG
  covers charts, diagrams and gauges, costs nothing, and cannot fail to load.
- **Render first, fill in after.** Put the tag at the END of the body and give
  whatever depends on it a visible placeholder. The bytes arrive while the page
  is already on screen, so a document that waits before drawing anything shows
  an empty card for that whole window.
- **Always wire `onerror`.** The device can be offline the first time a blob is
  needed. Replace the placeholder with a short plain-language line ("chart
  unavailable"), and keep the rest of the page useful without it.

#### If you do load a charting library

- **Draw into a `<canvas>`** sized by its container, with `responsive: true`
  and `maintainAspectRatio: false`. The card reflows live between its inline
  and full-screen sizes; a fixed-size chart is wrong in one of them.
- **Set `animation: false`** unless the motion carries meaning. The reader can
  reload the card, and a chart that replays its entrance every time reads as
  jank.
- Legend and tooltips are drawn into the canvas, so they behave as documented.
  Anything needing a font file does not — leave `font.family` on a system stack.

**Say where the numbers came from.** A chart reads as far more authoritative
than the same figures in a sentence — a reader cannot tell a measured series
from one you inferred. Name the source next to the chart (the tool output, the
file, the user's own message), and if a series is estimated, label it as
estimated in the chart itself. A well-drawn chart of numbers you guessed at is
the worst thing this skill can produce.

## Publish it

1. Keep the file at or below 16 MiB, then call `PutBlob` with its
   absolute path, `mime_type: "text/html"`, and `max_bytes: 16777216`.
   Do not use `AttachFile` for this preview.
2. Read `blob_id` from the tool's JSON result and put it unchanged into
   one `baybo-html` fenced block in the final response. Never paste the
   raw HTML, synthesize or alter an id, expose the rest of the tool
   result, or add an attachment.

The marker must have exactly this shape; the tool supplies the real id:

````markdown
```baybo-html
sha256:<64 lowercase hex>.<lowercase hex read token>
```
````

A short explanation may accompany the marker.

If `PutBlob` fails, fix what it reports — an oversize file, a bad path —
and try once more. If it still fails, say so plainly and answer without a
preview; do not fall back to pasting the HTML into the reply.
