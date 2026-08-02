---
name: html-gen
version: 0.1.0
description: "Create and publish a self-contained interactive HTML page in the iOS conversation. Use for visual explainers, diagrams, dashboards, mini apps, simulations, rich reports, or any request that is better experienced as a rendered webpage than plain text."
channels:
  - owner
argument-hint: "[page request]"
allowed-tools:
  - Read
  - Write
  - Edit
  - PutBlob
---

# Generate an HTML preview

Create the page requested by the user. For `/html-gen`, treat the text
after the command as the request; if it is empty, ask what to build.

## Write the page

Write one complete HTML document to an absolute path in the workspace,
with all CSS and JavaScript inline and graphics as inline SVG or `data:`
images.

### The sandbox it runs in

The document is served to an `<iframe sandbox="allow-scripts">` — no
`allow-same-origin` — under exactly this response CSP:

```
default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline';
img-src data:; connect-src 'none'; frame-src 'none'; object-src 'none';
base-uri 'none'; form-action 'none'
```

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
  `EventSource` and `sendBeacon` are dead. The page must carry every byte
  it needs.
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
