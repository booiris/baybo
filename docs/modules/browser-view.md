# browser-view

`baybo-browser-view` carries the dashboard's live, view-only (M1) picture of
the agent's browser. The browser sidecar streams CDP `Page.startScreencast`
frames to the gateway over a private unix-socket **link**, and the gateway
fans them out to web viewers on `GET /v1/browser/view/ws`. Taking control of
the browser (M2) is not built: the contract reserves `RequestControl`, and
the gateway always answers it with `takeover_disabled`.

## Topology

```
                         one shared browser for every session
 ┌───────────────┐  CDP (pipe, or browserUrl   ┌──────────────────────────┐
 │ Chrome        │◀────────────────────────────│ browser sidecar (node)   │
 │ host / docker │  in docker / cdp_url mode)  │  CDDM tools + proxy      │
 └───────────────┘                             │  cddm_tap → ScreencastHub│
                                               │  ViewLink                │
                                               └────────────┬─────────────┘
                                   unix stream socket, sidecar dials,
                                   gateway listens (bound before spawn)
                                               ┌────────────▼─────────────┐
                                               │ gateway                  │
                                               │  listener → BrowserViewHub│
                                               │  /v1/browser/view/ws     │
                                               └────────────┬─────────────┘
                                         WS, admin token, ≤ MAX_VIEWERS
                                               ┌────────────▼─────────────┐
                                               │ web dashboard  #/browser │
                                               └──────────────────────────┘
```

The sidecar runs on the host in every mode; in docker mode only Chrome is in
the container, and the screencast rides the same CDP connection CDDM already
uses. The container never sees the link.

## What the crate owns

| Module    | Role |
|-----------|------|
| `wire`    | The whole contract: `LinkUp` / `LinkDown` (sidecar ↔ gateway), `ViewerDown` / `ViewerUp` (gateway ↔ web), `FrameHeader`, states and reason codes. ts-rs exports it to `sidecars/tool/browser/src/generated/` (regen with `cargo test -p baybo-browser-view --features ts-export --lib wire::export_bindings`; gated by `scripts/check-ts-bindings.sh`). The web app mirrors it by hand in `app/web/src/api/browserViewTypes.ts`, and `browserViewSentinel.ts` fails `tsc` on any drift. |
| `codec`   | Bounded link framing (below). Lengths are checked before anything is buffered. |
| `limits`  | Every size and timeout (table below). The sidecar learns the link limits from `HelloAck`. It still copies the protocol version, kind bytes and `MAX_FRAME_HEADER_BYTES` in `view_link.ts`, and its tests copy the `HelloAck` limits; `limits::tests` reads those files and fails when a copy drifts. |
| `clamp`   | Private. The gateway's own enforcement of the limits it announced: tab count, id / url / title / error lengths (UTF-16 units). The sidecar clamps first; this is the second check. |
| `params`  | `BrowserLinkParams::resolve`: the socket path (`<state>/browser-link/link.sock`, falling back to `$XDG_RUNTIME_DIR/baybo/<hash>.sock` and then `/tmp/baybo-<uid>/<hash>.sock` when `sun_path` is too short) plus a fresh random secret. It is resolved once per process. |
| `listener`| Private. Binds the socket (`0700` dir, `0600` socket, stale socket replaced, a symlinked or foreign-owned dir refused), checks the peer uid, runs the handshake, and sends `StartScreencast` / `StopScreencast` on viewer edges. Link tasks live in the listener's `JoinSet`, so they end with it. |
| `hub`     | `BrowserViewHub::from_config` binds the listener **before** the sidecar spawns and hands back the listener task for the gateway's `TaskTracker`. Its `BrowserLinkConfig` is `Off`, `Failed` (no usable socket path, or no browser bundle in this build) or `Listen(params)`. The gateway gets only `BrowserViewer::subscribe() -> ViewSubscription` (a verb, not the store). |

## Link protocol

Framing (`codec.rs`): `[u32 BE total_len][u8 kind][payload]`, where
`total_len` counts the kind byte plus the payload.

- kind `0`: one JSON `LinkUp` / `LinkDown`, at most `max_json_bytes`.
- kind `1` (sidecar → gateway only): `[u32 BE hdr_len][FrameHeader JSON][JPEG]`.
  The gateway parses the header (`deny_unknown_fields`) to validate it, then
  forwards the whole payload to viewers byte for byte.

A framing error, an oversized message or a bad header drops the link; the
sidecar reconnects.

Handshake:

1. The sidecar connects and sends `Hello{protocol, secret, boot_id, pid,
   capabilities}` first. The gateway checks the peer uid on accept and waits
   `HELLO_TIMEOUT` for it.
2. The gateway answers `HelloAck{protocol, limits}` (the sidecar adopts the
   limits) or `HelloReject{reason}`. Checks run in order: `unauthorized`
   (not a `Hello`, or a secret that fails the constant-time compare),
   `protocol_mismatch`, `already_connected`.
3. **First wins.** While one link is live every other `Hello` gets
   `already_connected`. The sidecar retries that with backoff (250 ms
   doubling to 10 s); `unauthorized` / `protocol_mismatch` stop its link for
   good. A dropped link is also redialled with the same backoff.

Three counters, three meanings:

| Id | Owner | Changes when | Used for |
|----|-------|--------------|----------|
| `boot_id` | sidecar, random UUID at process start | the sidecar process restarts | sent in `Hello`; the gateway does not act on it today |
| `link_epoch` | gateway hub | every accepted link | `ViewerDown::Link`. The hub drops everything the previous link sent (cached state, pending frame); a viewer resets its state on a new epoch, even when a restarted sidecar reuses a `browser_gen` |
| `browser_gen` | sidecar | its puppeteer `Browser` is replaced (docker heal, host relaunch); restarts at 0 with the sidecar | in `Status`, `Targets`, `Stream` and every `FrameHeader`, so a viewer can tell a frame of the old browser from one of the new |

## Sidecar tap: why it does not conflict with CDDM

The sidecar never opens its own CDP connection and never calls a CDDM tool.
`cddm_tap.ts` replaces the static `McpContext.from`, which CDDM's `index.js`
looks up at call time each time it adopts a `Browser`. The wrapper returns
CDDM's own promise untouched and hands the context to `ScreencastHub` in a
microtask inside `try/catch`. From the context the hub reads
`getPages()`, `getPageById(id)` and `getSelectedPptrPage()`, and opens a
separate `page.createCDPSession()` per followed page only while someone is
watching. It takes no CDDM mutex and does not touch `BrowserActivity`, so
viewer traffic never reads as agent activity to the watchdog.

The tap is installed only when the link env is present. It is fail-safe: a
CDDM version other than `TAPPED_CDDM_VERSION`, internals that fail to load,
or a missing `from` reports
`Availability{cddm_version_mismatch}` and leaves the tools untouched. The
`chrome-devtools-mcp/McpContext` import is externalised by
`esbuild.config.mjs` to the same file `index.js` imports, so the patch hits
the class CDDM actually uses (see `docs/sidecars.md`).

Verified in the M0.1 spike (CDDM 1.1.0, Chrome for Testing 152):

- The tap fires lazily (not before the first tool call) and again whenever
  CDDM replaces the browser.
- A side `createCDPSession()` plus detach leaves `list_pages` and the page
  count unchanged.
- Headless, headful under Xvfb and connect mode all report CDDM tabs as
  visible and stream at about 60 fps while animating; static pages emit
  one frame, then only on change.
- `take_screenshot` latency is unchanged with a concurrent screencast.
- Acking late does **not** throttle Chrome (a 100 ms ack delay still gave
  30 fps), so the sidecar enforces the frame rate itself.
- With `experimentalPageIdRouting`, CDDM page tools need `pageId`; that id
  is also what the sidecar uses to follow the agent's page.

## Frame flow and backpressure

One followed stream: the page the agent last used (`noteAgentPage`, from a
tool call's `pageId` or a `new_page` result), falling back to CDDM's
selected page. Each hop drops instead of queueing:

| Hop | Mechanism |
|-----|-----------|
| Chrome → sidecar | `Page.startScreencast` with `quality` 60, max 1280×1280, `everyNthFrame` = round(60 / fps). The cap is 10 fps (5 fps in host mode, where the CDP pipe and event loop are shared with CDDM). Every frame is acked at once. |
| sidecar rate cap | Frames that arrive before the next `1000 / fps` slot are dropped; the newest dropped one is kept and sent when the slot opens, so a page that goes still shows its final state. |
| sidecar → link | Once `socket.write()` returns false, frames are dropped (newest kept) until `'drain'`. JSON control messages are always written. |
| gateway hub → viewers | Frames go into a `watch`: one slot, newest wins. A slow viewer skips frames and never holds up the link. State messages go through a 64-deep `broadcast`; a viewer that lags past that is resynced from the cached snapshot. |
| gateway → WS | Each send has `WS_SEND_TIMEOUT`; a viewer whose send stalls longer is dropped. |
| browser → canvas | `FrameDecoder` decodes one JPEG at a time with a single pending slot (newest wins) and draws straight to a canvas without a React render. |

## Behaviour the rest of the system relies on

- **Off means off.** The view runs only when `BrowserConfig::view_enabled()`
  (`browser.enable && browser.view.enable`). When the view is off, or no link
  can come up (no usable socket path, no browser bundle, or the listener
  failed to bind), the sidecar gets no link env and behaves exactly as it did
  before the view existed. Viewers then see `Link{up:false}`, with
  `unavailable: browser_disabled` only in the off case.
- **Sidecar env.** `BAYBO_BROWSER_LINK_SOCKET` (absolute socket path) and
  `BAYBO_BROWSER_LINK_SECRET` (constants in
  `crates/tools/src/mcp/profile/browser.rs`). Both are needed; either one
  missing turns the feature off in the sidecar. They are stable for the
  process, so the reconciler's env hash does not churn.
- **Viewer edges drive the stream.** A 0→1 viewer edge sends
  `StartScreencast`, and so does a new link while viewers are waiting. A 1→0
  edge sends `StopScreencast`, and the hub forgets the cached `Stream` (the
  sidecar sends no state on stop, so a cached `live` would greet the next
  viewer with a target the agent may have closed). A viewer never launches
  Chrome: before the first browser use the sidecar reports
  `Stream{state: idle}`.
- **Snapshot on join.** A new viewer first gets the cached `Link` / `Status`
  / `Targets` / `Stream`, then live updates and frames.
- **Browser replaced.** A disconnected browser pauses the stream
  (`Stream{state: paused}`) until the tap delivers a new one; nothing is
  reopened on the dead one.
- **No link liveness probe.** A dead sidecar closes its socket and frees the
  slot at once. A wedged one (event loop stuck, socket open) holds it until
  the MCP reconciler kills it; meanwhile its replacement retries
  `already_connected` with backoff.

## Viewer WS — `GET /v1/browser/view/ws`

- Admin-authed (Bearer or `?token=`; the token is stripped before the
  `TraceLayer` logs the URI). Only `Web` / `Device` clients may upgrade;
  others get 403. No OpenAPI entry, like `/v1/channel-ws`.
- Text messages are JSON. Down: `Link{up, link_epoch, unavailable,
  ping_interval_ms}`, `Status`, `Targets`, `Stream`, `Error{code}`, `Pong`.
  Up: `Ping`, `RequestControl`.
- Binary messages (down only) are exactly the link's frame payload,
  `[u32 BE hdr_len][FrameHeader JSON][JPEG]`.
- The client must `Ping` every `ping_interval_ms`. Two intervals without one
  closes the socket with 1008. Over `MAX_VIEWERS`: `Error{too_many_viewers}`
  then 1008. Gateway shutdown: 1001. An unparsable text message or any
  binary message from the client gets `Error{bad_message}`;
  `RequestControl` gets `Error{takeover_disabled}`.

## Trust model

- **Viewer side.** Anyone holding the admin token (or a paired device) sees
  the browser. There is one shared browser for every session, so a viewer
  sees whatever any session's agent is doing, not just one chat.
- **Link side.** The socket is `0700` / `0600`, the peer uid must match, and
  `Hello` must carry the secret. The secret is registered with the leak
  detector (`browser.link_secret`), and the sidecar deletes both variables
  from its own `process.env` before anything else runs, so Chrome and other
  children do not inherit them.
- **Same-uid caveat.** Deleting from `process.env` does not hide the secret
  from the same uid: the kernel keeps the original environ block, readable
  at `/proc/<pid>/environ` by any process of that uid (the agent's
  unsandboxed shell, or a Chrome renderer under `browser.sandbox=false`). In
  M1 such a process could at most kill the sidecar, win the reconnect race
  and show forged frames. Before M2 sends input over the link, the secret
  must move off the environment (an inherited fd or the first stdin line).
- **Docker mode is unchanged.** The container's CDP port (published on
  `127.0.0.1::9223`) and the optional noVNC port (`docker.web_vnc_port`,
  published on host loopback) are unauthenticated, as before. The live view
  does not use either; noVNC remains a docker-only debug path.

## Config

| Key | Default | Notes |
|-----|---------|-------|
| `browser.view.enable` | `true` | No effect while `browser.enable` is off. Elided from `baybo.json` while default. |

Restart to apply: the link params, the env handed to the sidecar and the
hub are all fixed at gateway boot.

## Limits (`limits.rs`)

| Constant | Value | Where it bites |
|----------|-------|----------------|
| `BROWSER_LINK_PROTOCOL_VERSION` | 1 | `Hello` / `HelloAck`; mismatch → `protocol_mismatch` |
| `MAX_SCREENCAST_FRAME_BYTES` | 2 MiB | JPEG per frame (`max_frame_bytes`) |
| `MAX_FRAME_HEADER_BYTES` | 4096 | `FrameHeader` JSON |
| `MAX_LINK_JSON_BYTES` | 256 KiB | one JSON link message (`max_json_bytes`) |
| `MAX_LINK_TARGETS` | 32 | tabs per `Targets`; extras dropped |
| `MAX_TARGET_ID_CHARS` | 64 | longer target ids are dropped, not truncated |
| `MAX_TARGET_URL_CHARS` | 512 | URL truncated |
| `MAX_LINK_TEXT_CHARS` | 256 | title / `Failed.error` truncated |
| `HELLO_TIMEOUT` | 5 s | link must send `Hello` |
| `MAX_VIEWERS` | 8 | concurrent viewers per gateway |
| `MAX_BROWSER_VIEW_CLIENT_MSG_BYTES` | 16 KiB | viewer → gateway text message |
| `VIEWER_PING_INTERVAL_MS` | 15 000 | announced in `Link` |
| `VIEWER_PING_TIMEOUT` | 30 s | viewer closed with 1008 |
| `WS_SEND_TIMEOUT` | 10 s | one send to a viewer |

Char limits are UTF-16 code units (JS `string.length`).

## Known gaps (M1)

- A link that cannot come up (bind failure, no socket path, no browser
  bundle) looks the same as "sidecar not connected yet"
  (`Link{up:false, unavailable:null}`). A dedicated `UnavailableReason`
  would be a contract change.
- `TapNotFired` is defined but never sent.
- `boot_id` is received but unused by the gateway.
- The route is also reachable through the relay tunnel. A WebSocket upgrade
  cannot succeed there, so it fails harmlessly.
- After the browser launches, the stream stays `idle` until the agent's next
  browser tool call: CDDM creates its context lazily, and the tap fires only
  then.
