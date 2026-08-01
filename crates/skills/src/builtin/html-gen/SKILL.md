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

1. Write one complete HTML document to an absolute path in the workspace.
2. Keep all CSS and JavaScript inline. Do not use external URLs, CDNs,
   network requests, nested frames, cookies, or browser storage. Use inline
   SVG or `data:` images when graphics are needed.
3. Make the page responsive for a phone-sized iframe, include a viewport
   meta tag, and preserve keyboard and screen-reader usability. Keep state
   in memory because the iframe has an opaque origin.
4. Keep the file at or below 16 MiB. After the page is complete, call
   `PutBlob` with its absolute path, `mime_type: "text/html"`, and
   `max_bytes: 16777216`. Do not use `AttachFile` for this preview.
5. Read `blob_id` from the tool's JSON result and put it unchanged into one
   `baybo-html` fenced block in the final response. Never paste the raw HTML,
   synthesize or alter an id, expose the rest of the tool result, or add an
   attachment.

The marker must have exactly this shape; the tool supplies the real id:

````markdown
```baybo-html
sha256:<64 lowercase hex>.<lowercase hex read token>
```
````

A short explanation may accompany the marker.
