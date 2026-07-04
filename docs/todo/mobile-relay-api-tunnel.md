# Mobile Relay API Tunnel

**Status:** implementation plan / migration in progress.

The device relay path currently has a sharp split:

- Chat uses the paired device's Noise-protected content leg and exchanges
  `wire::Frame`s.
- Blob upload/download uses dedicated relay blob legs. This migration replaces
  the old bespoke `device_proto::blob` request/response protocol with the API
  tunnel on those same background legs.
- Direct mode can call gateway REST endpoints such as `GET /v1/chat/sessions`,
  but relay mode cannot list sessions because the relay wire protocol has no
  list operation and the phone cannot reach the NAT'd gateway's HTTP listener.

This doc proposes a general **API tunnel** for paired mobile clients: the app
presents URL-shaped requests internally, while the transport carries raw
binary request/response streams over dedicated WebSocket relay data legs wrapped
in the existing Noise IK device authentication.

The goal is to solve session listing without adding chat `Frame` variants, and
to make blob transfer ordinary streaming HTTP-shaped traffic over the same
tunnel protocol.

## Goals

1. Let relay-bound device clients call a small, mobile-safe API surface that looks like
   HTTP from the app side.
2. Avoid adding `wire::Frame` variants for session listing, session metadata, or
   blob chunks.
3. Keep the relay host content-blind. It should see only the leg class, timing,
   and ciphertext sizes, never paths, blob ids, body bytes, device identity, or
   response data.
4. Preserve the current interactive-chat isolation: large uploads/downloads must
   not block the chat socket, the chat pump, or the relay bandwidth headroom.
5. Reuse existing REST DTOs and store/service code where possible, but do not
   expose the admin REST surface wholesale to the paired device.
6. Keep session rows durable. The tunnel must not introduce background cleanup,
   session-row deletion, or hidden retention behavior changes.

## Non-goals

- This is not a general-purpose HTTP proxy from the phone to localhost.
- This is not an admin-token tunnel. The paired device is authenticated by Noise
  IK and must be mapped to a constrained device principal.
- This does not replace the live chat `wire::Frame` stream. Streaming assistant
  output, user sends, approvals, turn state, and catch-up stay on the chat leg.
- This does not require a local loopback HTTP server in the first iteration. The
  app can expose URL-shaped calls through FFI first, then add a local URL facade
  only if a WebView or shared client stack needs one.

## Why not reuse chat frames?

`Frame::Subscribe` and `Frame::FetchHistory` both require the client to already
know a `session_id`. They cannot discover unknown sessions.

`Frame::SessionUpdated` and `Frame::SessionActivity` could be extended as a
metadata event stream, but that solves only "push known rows to the list". It
does not create a request/response API for initial list fetches, folders, model
metadata, blob bodies, or future mobile REST features.

Blob chunks also should not ride the chat frame stream. The existing blob design
intentionally uses separate relay legs so bulk transfer does not head-of-line
block chat and so the relay can meter blob as background traffic while keeping
one tenant bandwidth wall.

The tunnel keeps the same separation: chat remains `wire::Frame`; API and blob
HTTP use a different sub-protocol on separate Noise legs.

## Shape

### Leg classes

Reuse the existing relay content topology:

```text
phone /content/join/{relay_node_id}
  -> remote host blind splice
  -> gateway /content/host/{relay_key}
  -> Noise IK authentication
  -> sub-protocol selected by leg class
```

Keep `x-relay-leg-class` and extend the meaning of the class at the gateway:

- `chat`: existing `wire::Frame` chat loop, interactive bandwidth.
- `api`: tunnel requests with small/interactive bodies, interactive bandwidth.
- `blob`: tunnel requests for `/v1/blobs/*`, background bandwidth with chat
  headroom.

If adding a new relay class is too much churn, `api` can initially reuse the
interactive `chat` class at the relay while the gateway selects the API
sub-protocol from an opening byte/message after Noise. Blob should keep its
background class.

### Tunnel protocol

Add a new transport-agnostic sub-protocol, for example
`device_proto::api_tunnel`. It is not a `wire::Frame` variant.

Logical messages are encoded with named MessagePack and then passed through the
same Noise `write_chunked` / `FrameReassembler` layer used by chat and blob.
Large HTTP bodies are split into explicit body chunks.

```rust
pub enum ApiTunnelRequest {
    RequestHead {
        request_id: u64,
        method: String,
        path: String,
        headers: Vec<ApiHeader>,
        body_len: Option<u64>,
    },
    RequestBody {
        request_id: u64,
        offset: u64,
        data: Vec<u8>,
        last: bool,
    },
    Cancel {
        request_id: u64,
        reason: String,
    },
}

pub enum ApiTunnelResponse {
    ResponseHead {
        request_id: u64,
        status: u16,
        headers: Vec<ApiHeader>,
        body_len: Option<u64>,
    },
    ResponseBody {
        request_id: u64,
        offset: u64,
        data: Vec<u8>,
        last: bool,
    },
    Error {
        request_id: u64,
        status: u16,
        code: String,
        message: String,
    },
}
```

Initial implementation can be one request per leg, matching today's blob
transfer model. That avoids multiplexing and keeps flow control simple. A later
version can keep a warm interactive API leg and multiplex many request ids if
latency becomes noticeable.

### App-facing API

Start with an FFI function that looks like an HTTP request:

```rust
pub async fn api_request(
    method: String,
    path: String,
    headers: Vec<ApiHeader>,
    body: Vec<u8>,
) -> Result<ApiResponse, BayboError>
```

Direct mode can implement this with `reqwest` against the configured gateway.
Relay mode dials an API tunnel leg and streams the same logical request through
Noise.

Native Swift code can then call typed wrappers:

- `chat_list_sessions()`
- `chat_get_session(session_id, before_ordinal, limit)`
- `blob_upload_bytes(bytes, mime_type)`
- `blob_image(blob_id)`
- eventually, `update_apns_token(apns_token, apns_env)`

Those wrappers hide whether the active binding is direct or relay.

If a real URL facade is needed later, add one above the FFI:

- `WKURLSchemeHandler` for `baybo-relay://v1/...` in the transcript WebView.
- Or a loopback-only local HTTP server bound to `127.0.0.1` for native/client
  code that insists on `URLSession`.

The first iteration should avoid the local server unless it buys real reuse.

## Authorization model

Do not forward raw admin REST into the gateway.

The tunnel handler should construct a device-scoped principal from the Noise IK
authenticated device row. That principal is then authorized against a small
allowlist.

Recommended initial allowlist:

- `GET /v1/chat/sessions`
- `GET /v1/chat/sessions/{session_id}`
- `POST /v1/chat/sessions`
- `POST /v1/chat/sessions/{session_id}/token` only if the relay path ever needs
  channel-token compatibility, otherwise omit
- `GET /v1/blobs/{blob_id}`
- `POST /v1/blobs`

Defer these until the mobile UI actually needs them:

- `DELETE /v1/chat/sessions/{session_id}`. Even though this is a hide operation
  in Baybo, not a row delete, the mobile affordance should exist before exposing
  it.
- folder mutation endpoints
- model pinning
- `POST /v1/mobile/apns-token`, after the tunnel is stable enough to replace
  the current `Frame::UpdateApnsToken` opening frame
- cron-message/admin/status/analytics/config endpoints

The tunnel must reject:

- absolute URLs
- path traversal
- non-`/v1/...` paths
- hop-by-hop headers
- `Authorization` supplied by the phone
- arbitrary host headers

The gateway owns the upstream identity. The phone should not send an admin
Bearer token through the tunnel.

## Session listing semantics

The direct admin endpoint currently lists `ChannelType::http` chat sessions. A
relay-paired phone creates and sends sessions as `ChannelType::device`. The tunnel
must not blindly call the admin handler and return only HTTP rows unless that is
the intended product behavior.

Define an explicit mobile session-list service with the same response shape as
the web list:

```text
GET /v1/chat/sessions
```

Under an admin/direct principal, this can preserve today's HTTP-channel
behavior. Under a paired-device principal, it should list the sessions visible to
that device. The likely first rule is:

- include non-hidden `ChannelType::device` user sessions for the approved device's
  gateway;
- exclude cron-triggered sessions by default, matching the web sidebar;
- omit empty draft rows unless they are pinned or have a user preview, matching
  the current device `SessionIndex.merge(remote:)` filtering behavior;
- sort newest-first at the service boundary, while the device list can keep its
  local pinned-first projection.

Open product question: should a paired phone also list the user's web/direct
`http` conversations, or only its own `device` conversations? The implementation
should make this policy explicit in one place rather than relying on the
underlying REST endpoint's channel filter.

## APNs token update migration

`Frame::UpdateApnsToken` is an APNs-specific chat frame today. The app sends it as an
opening best-effort frame on every relay content connect so the gateway can keep
the paired device's APNs token fresh across reinstall, restore-from-backup, and
Apple token rotation.

Unlike `WorkSnapshot` or `WorkReplay`, this is not an ordered chat transcript
event. It is an idempotent device-state update, so it is a good candidate for
the API tunnel once the tunnel exists.

Proposed tunneled shape:

```text
POST /v1/mobile/apns-token
{ "apns_token": "...", "apns_env": "sandbox" | "production" }
```

Semantics:

- relay mode calls this over an interactive API tunnel leg after launch,
  foreground, pairing completion, and successful chat reconnect;
- failures are logged at debug/info level and never fail chat connect;
- the gateway authenticates the device through the tunnel's Noise IK principal,
  not through a bearer token in the body;
- the handler updates the approved device row only when the token/env changed;
- re-registration with the remote push host remains gateway-owned and signed,
  matching today's `UpdateApnsToken` path.

Migration path:

1. Add the tunneled endpoint and gateway tests proving it updates the same
   persisted fields and triggers the same re-registration behavior as today's
   frame path.
2. In the same migration change, cut APNs clients over to the tunneled request and remove
   the opening best-effort `Frame::UpdateApnsToken` send from the relay chat
   connection.
3. Delete `Frame::UpdateApnsToken` and the pre-router special case that handles
   it on the device content leg.

Do not move work/progress recovery frames this way. `WorkSnapshot`,
`WorkReplay`, `TurnState`, `Message`, and live reasoning/tool frames are ordered
session-stream events and should stay on the chat frame stream.

## Blob migration

Migration order is direct cutover, not dual-send or dual-protocol compatibility.
The dedicated `blob` leg stays; only the plaintext sub-protocol inside Noise
changes.

1. Add the generic API tunnel with streaming request and response bodies.
2. Add relay-mode `/v1/blobs/{id}` download over a `blob` class tunnel leg.
   Preserve:
   - token-bearing `blob_id` authorization;
   - on-device cache keyed by content hash;
   - staged `.part` file writes;
   - digest verification against the `sha256:` prefix;
   - cancel/close behavior.
3. Add relay-mode `POST /v1/blobs` upload over a `blob` class tunnel leg.
   Preserve:
   - 100 MiB per-blob cap;
   - content hash validation;
   - mime metadata;
   - durable blob storage;
   - no session-row cleanup.
4. Remove the bespoke blob protocol in the same migration:
   - `device_proto::blob::{BlobRequest, BlobResponse}`
   - `app/mobile/core/src/blob.rs` protocol state that is specific to
     `BlobRequest`/`BlobResponse`
   - mobile relay blob client code that speaks the bespoke protocol
   - `crates/gateway/src/channel/blob_content.rs`

Do not remove:

- `WireAttachment` and attachment references in chat messages.
- `BlobStore` and `/v1/blobs/*`.
- dedicated background transfer legs.
- relay bandwidth classing and chat-priority headroom.

## Gateway implementation plan

1. Add `device_proto::api_tunnel`.
   - encode/decode helpers
   - chunking compatibility tests
   - request/response state helpers for contiguous offsets

2. Add a gateway API tunnel responder.
   - new `crates/gateway/src/channel/api_tunnel.rs`
   - shared blob operations in `crates/gateway/src/channel/blob_service.rs`
   - generic binary sink/source, matching the existing relay responder
     testability
   - Noise IK responder authentication, shared with device content
   - one request per leg in v1

3. Add a mobile API service layer instead of calling admin Axum handlers
   directly.
   - expose typed functions for list/create/get session, APNs token update, and
     blob operations
   - reuse DTO construction where possible
   - keep authorization explicit and local to the tunnel service

4. Wire relay control to choose the API responder.
   - use `LegClass::Api` for small interactive tunnel calls
   - keep blob calls on `LegClass::Blob`

5. Keep admin REST and direct mode stable.
   - direct mode may continue calling existing REST endpoints
   - relay mode gets equivalent typed wrappers through the tunnel

## Device / mobile client plan

1. Add an `ApiTunnelSession` beside `ContentSession`.
   - same Noise IK handshake
   - encode/decode `ApiTunnelRequest` / `ApiTunnelResponse`
   - one-shot request helper for JSON endpoints
   - streaming helpers for blobs

2. Add `BayboClient.api_request`.
   - direct arm: `reqwest` against the stored base URL
   - relay arm: dial API or blob tunnel leg based on path/body size

3. Refactor typed wrappers to use the request abstraction.
   - `chat_list_sessions()` no longer errors on relay
   - `chat_create_session()` can either keep client-minted relay ids or move to
     a tunneled `POST /v1/chat/sessions` once mobile session semantics are
     settled
   - cut `UpdateApnsToken` over to the tunneled token update in the final APNs
     migration change
   - `blob_upload_bytes()` and `blob_image()` move as part of the blob cutover

## APNs token migration

`Frame::UpdateApnsToken` remains on the chat content leg during the blob cutover.
Do not dual-send it. After the blob protocol has been removed and the tunnel is
stable, migrate APNs token refresh directly to a tunneled mobile endpoint such as
`POST /v1/mobile/apns-token`, then remove the frame variant and gateway
intercept in that APNs-specific change.

4. Keep `SessionIndex` as the list render source.
   - direct and relay both merge remote summaries into the local registry
   - local sends still update immediately for offline/local-first behavior
   - relay list refresh should happen on appear, foreground, and pull, matching
     direct

## Testing plan

Protocol tests:

- round-trip request/response heads with unknown headers ignored or rejected as
  intended
- streaming body chunks enforce contiguous offsets
- cancel closes or drains the in-flight request
- malformed paths and forbidden headers are rejected

Gateway tests:

- approved device can call `GET /v1/chat/sessions` over an in-memory API tunnel
- unapproved device cannot open the tunnel
- hidden sessions are excluded by default
- cron sessions are excluded by default
- the handler does not delete or mutate session rows during list refresh
- device principal cannot call non-allowlisted admin endpoints
- tunneled APNs token update persists the same device fields as
  `Frame::UpdateApnsToken`
- APNs token update failures do not fail chat reconnect
- blob download refuses an invalid read token
- blob upload enforces the size cap and deletes a mismatched content hash

Relay tests:

- `api` and `blob` legs use the same per-tenant bucket as chat
- `blob` legs reserve background bandwidth and preserve chat headroom
- relay logs never include path, blob id, request body, or response body

Device client tests:

- relay-bound `ChatListScreen.refresh()` populates from the tunnel
- direct-bound refresh still uses direct REST and preserves behavior
- local send still updates `SessionIndex` before the network round trip
- blob image cache still avoids re-downloads
- failed relay API request keeps the local list visible and logs only sanitized
  errors

## Rollout

Phase 1: blob-over-tunnel cutover

- implement API tunnel protocol with streaming bodies
- move relay blob download/upload to the tunnel on `LegClass::Blob`
- remove the bespoke blob protocol in the same change

Phase 2: JSON-only tunnel

- implement session-list service
- dial it with `LegClass::Api`
- make relay `chat_list_sessions()` work

Phase 3: session detail parity

- move relay transcript/detail fetches that are URL-shaped onto the tunnel where
  useful
- keep live chat and history paging frames if they are still better for ordered
  transcript replay

Phase 4: harden and test tunnel coverage

- cover blob digest mismatch, invalid read token, resume range, and upload cap
- cover relay logs and bandwidth class behavior

Phase 5: remove `Frame::UpdateApnsToken`

- add the tunneled `POST /v1/mobile/apns-token` endpoint
- switch APNs clients to the tunneled request in the same change
- remove the old opening frame and delete the APNs-specific `wire::Frame` variant

## Open questions

- Should relay-bound device clients list only `device` sessions, or should they show the user's
  `http` web/direct sessions too?
- Is a warm multiplexed API leg worth the complexity, or is one request per leg
  fast enough after relay preconnect?
- Should the tunnel path be exposed as `baybo-relay://...` to the WebView, or
  stay as typed Swift/Rust wrappers?
- Should small blob thumbnails ride an interactive API leg while full-size blobs
  use background, or should every `/v1/blobs/*` request be background for
  predictability?
- Should `POST /v1/mobile/apns-token` also be used by direct mode, or should
  direct mode keep its existing admin-token push-registration path?

## Related

- `docs/modules/mobile/companion.md`
- `docs/modules/mobile/blob-transfer.md`
- `docs/modules/mobile/relay-push-security.md`
- `app/ios/ffi/src/lib.rs`
- `app/ios/ffi/src/transport.rs`
- `app/ios/ffi/src/relay/blob.rs`
- `app/mobile/core/src/api_tunnel.rs`
- `crates/gateway/src/channel/device_content.rs`
- `crates/gateway/src/channel/api_tunnel.rs`
- `crates/gateway/src/channel/blob_service.rs`
- `crates/gateway/src/api/admin/chat.rs`
- `crates/wire/src/lib.rs`
