# Relay API Tunnel: Leg Reuse

**Status: implemented.** One relay API tunnel leg now serves many requests.

- **Stage 0** — the gateway request loop, `LegState`/`BodyDrain`, and the
  `TunnelReuse` wire field (`crates/gateway/src/channel/api_tunnel.rs`,
  `crates/device-proto/src/api_tunnel.rs`).
- **Stage 1** — the client's framing hygiene: `LegIo`, request-id checking, the
  frame queue, non-2xx draining, empty-body normalization, and the first
  per-request timeout (`app/ios/ffi/src/relay/{tunnel,api,blob}.rs`).
- **Stage 2** — the leg pool, the `.background` barrier, and `Replayable`
  (`app/ios/ffi/src/relay/leg_pool.rs`, `gateway_api.rs`, `App/BayboApp.swift`).
- **Stage 3** — the pre-dialed warm leg and the decoupled APNs POST
  (`leg_pool::warm`, `apns.rs`).

The "Today" sections below describe the behaviour this replaced, and are kept
because the reasoning only makes sense against it.

## What this replaced: one leg per request

On the relay leg, **every** gateway REST call dials a brand-new Noise tunnel.
`relay::api::request` (`app/ios/ffi/src/relay/api.rs:97`) does, per call:

1. `load_paired_record()` (keychain),
2. `dial_tunnel_leg` (`relay/tunnel.rs:20`) → `dial_content_join` (WSS to the
   relay, `relay/dial.rs:64`) → Noise IK handshake (msg1 out, msg2 in),
3. seal one `TunnelRequest::Head` (+ `Body` chunks),
4. read `TunnelResponse::Head` + `Body`,
5. `ws.close()` — the leg is thrown away (`api.rs:163`).

That is roughly **five phone-side round trips** (WSS upgrade ≈3, Noise IK ≈1,
Head/Body ≈1) for each of `chat_create_session`, `chat_list_sessions`,
`chat_fetch_sync`, `chat_lookup_message`, `chat_mark_read`, `chat_set_archived`,
`chat_set_pinned`, `chat_hide_session` (`app/ios/ffi/src/gateway_api.rs`,
dispatched through `GatewayJsonClient`).

The chat list refreshes on every appear and every foreground. Opening a chat runs
a sync. The first send of a draft creates the session. All of them pay the full
dial. The **direct** leg pays none of this — it holds a pooled `reqwest::Client`
(`app/ios/ffi/src/direct/mod.rs:100`).

The gateway side (`crates/gateway/src/channel/api_tunnel.rs`) mirrors the
one-shot shape: `run_tunnel_session` does the responder handshake, reads
**exactly one** request head under `TUNNEL_IDLE_TIMEOUT = 20s` (`:79-84`),
forwards it into `tunnel_http::router` (which injects
`Authorization: Bearer <device.auth_token>` + `x-baybo-device-id`, `:314-319`),
sends the response, and **returns** — the socket is dropped.

## Design

One dialled+handshaked leg serves N sequential requests, so a burst (list → sync
→ mark_read) and closely-spaced calls (create-on-first-send right after a list
refresh) pay the dial once.

### Constraints that shape everything below

1. **The Noise session is stateful and strictly ordered.** `ApiTunnelSession`
   (`app/ios/ffi/src/core/api_tunnel.rs:11`) is a `TransportState` plus a
   `FrameReassembler`, both `&mut`. Out-of-order decrypt is fatal. **Pipelining
   concurrent requests on one leg is impossible** without redesigning the crypto
   framing. Concurrency comes from *more legs*, never from sharing one.
2. **Version skew is permanent.** The iOS app ships through the App Store; the
   gateway is self-hosted and upgraded whenever the user feels like it. Both
   *new app + old gateway* and *old app + new gateway* must work.
3. **Blob legs share the same handler.** `LegClass::Blob` (`relay/blob.rs:35`)
   also calls `dial_tunnel_leg` and lands in `run_api_tunnel_over_relay`. Blob
   transfers run to 100 MiB / 10 minutes. They must not participate — see
   [Blob legs](#blob-legs-never-pooled).
4. **A leg may only serve another request if its framing is unambiguous.** If a
   request's declared body was not fully drained, or its response not fully sent,
   the leg must close.

### Capability negotiation: a typed field on `TunnelResponse::Head`

`crates/device-proto/src/api_tunnel.rs`:

```rust
/// The gateway declaring, on every response head, how long it will hold this leg
/// open for another request. Absent = one-shot leg (today's semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelReuse {
    /// Idle budget between requests, in ms. The client must close before it.
    pub idle_ms: u32,
}

pub enum TunnelResponse {
    Head {
        request_id: u64,
        status: u16,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<TunnelHeader>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body_len: Option<u64>,
        /// NEW — the only capability signal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reuse: Option<TunnelReuse>,
    },
    Body { .. },   // unchanged
    Error { .. },  // unchanged
}
```

**Not a response header.** `forwarded_response_headers` (`api_tunnel.rs:594-616`)
pushes *every* header the router produced into the same `Vec<TunnelHeader>`, and
a marker name would not be in `FORBIDDEN_HEADERS` (`:36-48`). Any `/v1` route,
sidecar, or reverse proxy emitting that name would let an **old** gateway
advertise a capability it does not have; the client would then write Head #2 into
a `run_tunnel_session` that has already returned, and hang until the request
timeout. Headers are also untyped, duplicable, and have no single source of truth
across the two Cargo workspaces. And `TunnelResponse::Error` has **no headers
field at all** (`device-proto/src/api_tunnel.rs:79-83`).

**Not the Noise msg2 payload.** It is genuinely free (the gateway writes `&[]`,
`device_content.rs:191`; the client reads and discards, `core/content.rs:37-40`),
so an added payload would *not* break old clients — but it would change
`responder_handshake`, which the **chat** leg also runs. Not worth the blast
radius for information that arrives one round trip earlier than the response head
does. Recorded here as the free slot to use *if* a future capability must be
known before request #1.

**No Ping/keepalive frame, ever.** `api_tunnel::decode::<TunnelRequest>` on an
unknown `type` tag is an `Err` (internally-tagged serde enum, no
`#[serde(other)]`), so `next_request` fails and the leg dies. A new variant may
only be sent *after* the peer is known to be new — and by then the only thing it
could buy is outliving the idle budget the server just declared. To widen the
window, raise `TUNNEL_REUSE_IDLE_TIMEOUT` — one constant, server-side, no
negotiation, no radio wakeup.

#### Both skew directions, byte by byte

**New app + old gateway.** The old gateway's response head is a msgpack map with
no `reuse` key → `#[serde(default)]` → `reuse: None` → the client drains the body,
closes the socket, **does not pool**. Zero speculation, zero extra round trip,
zero retry. Byte-identical to today.

**Old app + new gateway.** The new gateway's head carries one extra map key. The
old client's `rmp_serde::from_slice::<TunnelResponse>` is a named map with no
`deny_unknown_fields` → the key is **silently ignored**. The old client reads its
body and closes. The gateway's loop sees EOF with `served > 0` and returns `Ok(())`
immediately (it does **not** hang for the idle budget). Any straggler frame from
the old client's empty-body encoding is dropped by the [straggler
drain](#the-loop).

### The gateway

#### Thread `LegClass` through

`run_api_tunnel_over_relay` does not currently know its own leg class, but
`relay_content.rs:532-542` has it at the `match` that collapses `Api | Blob` into
one handler. Pass it down:

```rust
// relay_content.rs
LegClass::Api | LegClass::Blob => {
    drop(ah_rx);
    run_api_tunnel_over_relay(ws, class, state).await;
}
```

**`LegClass::Blob` keeps today's single-request path, byte for byte** — no loop,
no lifetime cap, and it always answers `reuse: None`, so "blobs are never pooled"
is a **server-side property**, not a client-side promise.

#### Constants

```rust
const TUNNEL_IDLE_TIMEOUT: Duration = Duration::from_secs(20);        // unchanged: waiting for the FIRST head
const TUNNEL_REUSE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);  // NEW: waiting for a SUBSEQUENT head; also the value of reuse.idle_ms
```

There is deliberately **no** `MAX_REQUESTS_PER_LEG` and **no**
`MAX_LEG_LIFETIME`. The only argument for them — that the `AuthenticatedDevice`
snapshotted at handshake would be replayed indefinitely — is false: every forward
runs `require_admin_token` inside `tunnel_http::router` (`tunnel_http.rs:15-29`),
so a revoked token takes effect on the next request. Their cost is real: two
close conditions that are not declared on the wire, which turns "the client
always closes first" into a lie and forces retry to become the primary mechanism
rather than the safety net. One budget, one wire field, one invariant. (A hostile
client capped this way just re-dials; caps stop nobody.)

#### The loop

```rust
enum LegState { Reusable, MustClose }

let (mut transport, device) = responder_handshake(&mut sink, &mut source, state).await?;
let mut reassembler = FrameReassembler::new();
let mut pending = VecDeque::new();
let mut served: u64 = 0;
let mut last_id: u64 = 0;

loop {
    let idle = if served == 0 { TUNNEL_IDLE_TIMEOUT } else { TUNNEL_REUSE_IDLE_TIMEOUT };
    let next = match tokio::time::timeout(
        idle, next_request(&mut source, &mut transport, &mut reassembler, &mut pending)).await
    {
        Err(_) if served > 0 => return Ok(()),        // a reused leg idling out is a normal close
        Err(_) => return Err("timed out waiting for tunnel request".into()),
        Ok(Err(reason)) => return Err(reason),        // Noise/decode failure is fatal
        Ok(Ok(None)) if served > 0 => return Ok(()),  // old client closed after its one request — exit at once
        Ok(Ok(None)) => return Err("peer closed before request head".into()),
        Ok(Ok(Some(req))) => req,
    };

    let head = match next {
        TunnelRequest::Head { request_id, .. } if request_id <= last_id => {
            send_error(&mut sink, &mut transport, request_id, 400, "request id must increase").await?;
            return Ok(());
        }
        TunnelRequest::Head { request_id, method, path, headers, body_len } => {
            last_id = request_id;
            RequestHead { request_id, method, path, headers, body_len }
        }
        // STRAGGLER DRAIN: a late frame for an already-served request. Its only
        // sources are the historical client's empty-body encoding (Head{body_len:
        // Some(0)} + Body{last:true}, which the gateway's no-body branch never
        // reads) and a late Cancel. Framing is clean here, because every path that
        // ABANDONS a body drain has already gone MustClose.
        TunnelRequest::Body { request_id, .. } | TunnelRequest::Cancel { request_id, .. }
            if request_id <= last_id => continue,
        TunnelRequest::Body { request_id, .. } => {
            send_error(&mut sink, &mut transport, request_id, 400, "body sent before request head").await?;
            return Ok(());  // a body for an unknown id ⇒ framing is unknowable ⇒ close
        }
        TunnelRequest::Cancel { reason, .. } => return Err(format!("request canceled: {reason}")),
    };

    if let Err((status, reason)) = validate_request_head(&head) {
        send_error(&mut sink, &mut transport, head.request_id, status, reason).await?;
        return Ok(());
    }

    match handle_http_forward(.., head).await? {
        LegState::Reusable => served += 1,
        LegState::MustClose => return Ok(()),
    }
}
```

#### The desync rule

> **A leg may serve another request only if the declared body was fully drained
> (or there was none) AND the response was fully sent. Every other exit closes.**

Plus one blanket rule for the client's benefit: **any `TunnelResponse::Error` the
gateway sends means the leg is dead.** This is deliberate. `Error` has no headers
field, so it cannot carry `reuse`; keeping "a body-less 403 may continue" would
force the client to re-implement the gateway's decision table *in another Cargo
workspace* (quirks included — e.g. `request_body_len`'s `.ok().flatten()`
swallowing a malformed content-length). Dropping that one row collapses the client
rule to a single line: **"an `Error` means throw the leg away."** It costs only a
re-dial after a request that should never have been sent.

| Exit point (existing code) | LegState |
|---|---|
| No-body head, `send_http_response` returns `Ok` | **Reusable** |
| `validate_request_head` fails (`:117`) | MustClose |
| `build_forward_request` fails, no body (`:196-202`) | MustClose |
| Body head: 413 precheck (`:222-232`) | MustClose (the client may still be uploading) |
| Body head: `build_forward_request` fails (`:238-244`) | MustClose |
| Body head: `stream_forward_body` returns `Err` (offset mismatch / overlong / short / chunk-idle / total timeout / Cancel) | MustClose |
| **Body head: `stream_forward_body` returns early at `tx.send(..).is_err()` (`:390-392`) — the router hung up on the body (e.g. a 401 before any extractor read it)** | **MustClose** (still send the response) — **this is the live detonator** |
| Body head: body drained + `send_http_response` `Ok` | **Reusable** |
| `send_http_response` errors mid-way | `Err` ⇒ abort (already `?` today) |
| SSE (`body_len: None`, terminated by an empty `last:true`, `:552-591`) | Reusable (the phone never initiates one) |

Signatures become:

```rust
enum BodyDrain { Drained, Abandoned }   // Abandoned = the router hung up on the body

async fn stream_forward_body(..) -> Result<BodyDrain, (u16, String)>;  // :390-392 returns Abandoned
async fn handle_http_body_forward(..) -> Result<LegState, String>;     // Abandoned ⇒ still send_http_response, then MustClose
async fn handle_http_forward(..) -> Result<LegState, String>;
```

`Abandoned` must still await `response_task` and send the response — the client
needs that 401 — and only *then* close.

`reuse` is injected at `send_http_response`'s three `TunnelResponse::Head`
construction sites (`:424`), as `Some(TunnelReuse { idle_ms: … })` **only when
`class == LegClass::Api`**.

### Client: hygiene first, then the pool

These three bugs exist **today**. They are survivable only because the leg dies
after one request. Reuse turns each of them into a silently wrong answer, so they
must land in the same batch.

**Empty-body encoding (`relay/api.rs:122-135`, `relay/blob.rs:151-162`).** Delete
the `if body.is_empty()` arm: **`Some(empty)` is no body** — `body_len: None`, no
`Body` frame. The gateway's `request_body_len()` already folds `Some(0)` to 0 and
takes the no-body branch, which **never reads that frame** (`api_tunnel.rs:182,
645-649`). Today the socket close reclaims it; on a reused leg it would pop out of
the *next* `next_request`. The straggler drain is the server-side seatbelt (and the
compat path for already-shipped clients), but the client must stop sending it.

**Response reader becomes request_id-aware, with a frame queue
(`relay/tunnel.rs`).**

```rust
pub(crate) struct LegIo {                 // replaces the bare (WsStream, ApiTunnelSession)
    pub(crate) ws: WsStream,
    pub(crate) session: ApiTunnelSession,
    pending: VecDeque<TunnelResponse>,    // mirrors the gateway's: one Noise message can hold several frames
    pub(crate) next_request_id: u64,      // from 1, +1 per request
}
```

- `next_response` must push the **whole `Vec`** from `session.open()` into
  `pending`. Today it takes `into_iter().next()` and **silently drops the rest**
  (`tunnel.rs:70-78`) — which holds only because the gateway happens to seal one
  frame per WS message. That is a coupling, not an invariant.
- `expect_response_head` / `collect_response_body` must **check `request_id`**.
  Today both discard it with `..` (`:87-96, 112-126`) *and* silently skip
  wrong-typed frames. Those two lenienies compose into exactly the bug that feeds
  request N's leftovers to request N+1. A mismatch is `LegError::Desync` ⇒ kill the
  leg, **no retry**.
- Keep `REQUEST_ID` (it is imported by `blob.rs:15`), renamed
  `pub(crate) const BLOB_REQUEST_ID: u64 = 1;` — one-shot blob legs only.

**Non-2xx must drain (`relay/api.rs:157-161`).** Today a non-2xx closes and
returns **without reading the body**. The gateway does not care about status: it
still sends `Head{404, body_len: Some(43)}` + `Body{43B, last:true}`. And a 404 from
`chat_lookup_message` is the **normal** result of an outbox rebase, not an edge
case. New rule:

```
Head.status non-2xx  ⇒ drain the body first, then return Err; the leg is still poolable per `reuse`.
TunnelResponse::Error ⇒ the leg is dead ⇒ discard, never pool.
```

> The id check and the drain rule **must ship together**. Id check without drain:
> every 404 poisons the next request. Drain without id check: retry gate 2 below
> is unimplementable.

### Client: the leg pool

New file `app/ios/ffi/src/relay/leg_pool.rs`. **Wait-free, K-deep, serialized by
ownership.**

```rust
pub(crate) const MAX_POOLED_LEGS: usize = 3;
const CLIENT_IDLE_MARGIN:  Duration = Duration::from_secs(10);
const CLIENT_IDLE_CAP:     Duration = Duration::from_secs(120);
const UNPROVEN_LEG_TTL:    Duration = Duration::from_secs(12);
const TUNNEL_REQUEST_TIMEOUT:        Duration = Duration::from_secs(30);
const POOLED_LEG_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(4);

struct PooledLeg {
    io: LegIo,
    binding: BindingKey,                // relay_node_id + gateway_static_pubkey + device_id
    epoch: u64,
    reuse: Option<TunnelReuse>,         // None = unproven (warmed, never used)
    parked_mono: std::time::Instant,    // monotonic — does NOT advance across device sleep
    parked_wall: std::time::SystemTime, // wall — DOES advance across sleep
}

pub(crate) struct ApiLegPool {
    inner: parking_lot::Mutex<PoolInner>,   // guards the Vec and the epoch; NEVER held across an await
}
```

**The lock is not the serializer — ownership is.** `take()` *moves* a leg out of
the `Vec` and hands it to one caller; an `ApiTunnelSession` is therefore only ever
touched by one request. Constraint 1 holds by construction. The `parking_lot::Mutex`
is only held inside the three synchronous operations (`take` / `park` /
`invalidate`), so no `tokio::sync::Mutex` is needed.

**Callers never queue.** `take()` returns `None` → dial your own leg, immediately.

A leg is usable iff ① `leg.epoch == inner.epoch`, ② `leg.binding == key`, and
③ it is inside its TTL:

- **proven** leg: `ttl = min(reuse.idle_ms, CLIENT_IDLE_CAP) - CLIENT_IDLE_MARGIN`
  (a 60s gateway ⇒ 50s);
- **unproven** leg (only from `warm()`): `ttl = UNPROVEN_LEG_TTL = 12s`. The
  gateway's budget for the *first* head is `TUNNEL_IDLE_TIMEOUT = 20s` on **both**
  versions, so a dialled-but-unused leg is reclaimed at 20s regardless of what the
  peer supports. 12s leaves 8s of headroom. **This is the entire reason pre-dialling
  needs no negotiation.**
- **Both clocks must pass.** On Darwin `std::time::Instant` is `mach_absolute_time`
  and **does not advance while the device sleeps** — an overnight leg would claim to
  be three seconds old. A large wall delta with a small monotonic delta *is* the
  signature of a suspended process, and is on its own enough to condemn the leg. (A
  user rewinding the clock makes `SystemTime::elapsed()` return `Err` ⇒ treat as
  unusable. Safe direction.)

**Why K-deep and not one leg behind a contention valve.** The app's hot edges are
fan-outs, not serial chains: `ChatStore.reconcileOutboxOnConnect` spawns a
`chatLookupMessage` Task **per unconfirmed entry**, concurrently, while
`chatFetchSync`, a fire-and-forget `markRead`, and the APNs POST land alongside —
opening a chat is ~6 concurrent API calls. One leg plus a 150 ms valve hits 1/6 and
makes the other five *wait* 150 ms before dialling anyway — **strictly worse than
today**. A wait-free 3-deep pool hits 3/6 and the rest take today's path at zero
added latency: **never worse on any axis.**

**Per-request timeouts.** A pooled leg gets `POOLED_LEG_FIRST_BYTE_TIMEOUT = 4s`
to first byte (pinning the cost of a NAT blackhole / suspended zombie at 4s instead
of 30s), then `TUNNEL_REQUEST_TIMEOUT = 30s` overall. A freshly dialled leg keeps
today's dial budget (`dial.rs`'s 14×500 ms 503 retry + 15s handshake) then 30s.
**There is no per-request timeout today at all.**

On timeout: **discard the leg; never pool it; never send `TunnelRequest::Cancel`.**
`Cancel` is only handled *inside* `stream_forward_body` (`api_tunnel.rs:398-404`);
while the gateway awaits `router.oneshot(req)` or writes the response it is not
reading the source at all, so on the API path sending a Cancel and just hanging up
are the same thing to the gateway — and hanging up has one fewer failure mode.
`Cancel` stays what it is today: dead code on the request path, kept because it is
the only clean way to ever interrupt a long blob upload.

#### Retry: honestly at-least-once

```rust
pub(crate) struct LegFailure { pub sent_any: bool, pub response_seen: bool, pub was_pooled: bool }
```

**Retry iff `was_pooled && !response_seen && route.replayable == Replayable::Yes`.
At most once, always on a freshly dialled leg. A failure on a fresh leg is never
retried** (otherwise one real outage becomes a dial storm).

The tempting lemma — *"no Head received ⇒ the gateway did not execute"* — is
**false**. The gateway runs `router.oneshot(req)` first and `send_http_response`
second; dying between them (gateway restart, relay drop, iOS network path flip) is
exactly "pre-Head failure, side effect already committed". Therefore:

> **A pooled request is at-least-once. It is safe only because every route on this
> surface is idempotent.**

Make that a compile-time obligation rather than folklore, in
`app/ios/ffi/src/gateway_api.rs`:

```rust
/// A write that may be replayed after a transport failure: the server keys the
/// effect on CLIENT-SUPPLIED data (session_id / APNs token / blob digest), so a
/// replay converges to the same state. Replay is at-least-once — the gateway MAY
/// have already executed it.
pub(crate) enum Replayable { Yes, No }
```

| Route | Idempotent? | Why |
|---|---|---|
| `GET /v1/chat/sessions`, `…/{id}`, `…/sync`, `…/messages?platform_msg_id=` | ✔ | reads |
| `POST /v1/chat/sessions {session_id}` | ✔ `Yes` | `get_or_create`, keyed on the **client-minted** `session_id` |
| `PUT …/archive`, `…/pin` | ✔ | absolute assignment, not a toggle |
| `PUT …/read {ordinal}` | ✔ | server-side max-wins, monotonic |
| `DELETE …/{id}` | ✔ | soft `set_hidden(true)` (session rows are never deleted) |
| `POST /v1/mobile/apns-token` | ✔ `Yes` | upsert per device |
| **`POST /v1/blobs`** | ✘ `No` | **every put mints a fresh `blob_id`** (which is exactly why attachment dedup keys on the sha256 digest). Uploads go through `GatewayBlobClient` on a one-shot blob leg, never through `GatewayJsonClient`, so they cannot reach the pool at all — `Replayable::No` is the marker that makes wiring them in later **fail to compile** rather than double-run in the field. It is deliberately unconstructed today. |

Chat message sends never go through `GatewayJsonClient` — they ride the chat leg
as `Frame`s (`transport.rs`). There is no "duplicate user message" hazard on this
surface.

`direct/mod.rs` ignores the parameter (reqwest never retries); only the signature
changes.

### iOS backgrounding

**Do not hang the barrier off `relay_preconnect`.** Two independent reasons: (a)
`AppStore.swift:335-337`'s `guard !relayPreconnectInFlight else { return }` skips
the whole call while a previous preconnect is still on the network — i.e. exactly
in the weak-network case that needs the barrier most; (b) even when it does run, it
and `ChatListScreen.swift:100-104`'s `.onChange(of: scenePhase) { Task { await
refresh() } }` are **unordered sibling Tasks on the same edge**, and the refresh
routinely wins and takes the zombie leg first.

Three layers, in order of reliability:

**Layer 1 — a synchronous FFI barrier on `.background`.**

```rust
// app/ios/ffi/src/lib.rs — NOT async: no runtime hop, no await
pub fn relay_invalidate_api_legs(&self) {
    relay::leg_pool::pool().invalidate();   // parking_lot lock + epoch += 1 + drain(Vec)
}
```
```swift
// app/ios/App/BayboApp.swift
.onChange(of: scenePhase) { _, phase in
    if phase == .active { store.didBecomeActive() }
    if phase == .background { Baybo.client.relayInvalidateApiLegs() }
}
```

`.background`, not `phase != .active`: suspension always goes active → inactive →
**background**, but a notification banner, Control Centre, an incoming call, Face
ID, the share sheet, and the app-switcher peek only reach `.inactive` and bounce
back — the socket is untouched there and there is no reason to throw away a warm
leg. The epoch bump is **synchronous**, so it happens before any Task spawned later
in that scenePhase turn; a request already in flight (its leg on the caller's stack)
sees the stale epoch on completion and is **discarded rather than parked**.

**Layer 2 — the dual-clock parking stamp** (above). Catches the cases where the
`.background` callback never ran (jetsam, crash recovery, fast switching) and device
sleep.

**Layer 3 — `POOLED_LEG_FIRST_BYTE_TIMEOUT = 4s`.** The price ceiling on everything
that slips through. This is the **only new tail-latency mode** the design
introduces; today every call gets a fresh socket, and a fresh socket cannot be a
blackhole.

Two more things are required or the barrier is pointless:

**The APNs POST must leave the pool's critical path.** `lib.rs:358-361` awaits
`refresh_relay_apns_best_effort` **before** `transport::connect` — if that POST goes
through the pool, **every chat-leg subscribe queues behind the API pool**. And
`AppStore.swift:326` calls `scheduleReconnect()` on up to 12 stores at once on
foreground, i.e. 12 identical APNs POSTs hitting the pool together. This design
kills "REST over the chat leg" for head-of-line blocking; it must not build the
mirror image of that coupling. Fix: have `ApnsState` remember `last_posted: (token,
env)` and **skip the request entirely when unchanged** (the steady state);
`tokio::spawn` it when it does fire; stop gating `transport::connect` on it.

**Pre-dialling must be explicit.** The claim that `relay_preconnect` already dials a
leg for APNs (so a warm leg is free) is **false**:
`refresh_relay_apns_best_effort` returns without sending anything when there is no
APNs token (`lib.rs:707-711`) — the simulator, users who declined push, and the
window on every cold start before APNs hands the token back (`AppStore.swift:311-313`
re-arms registration on every foreground precisely because the app expects to often
have no token).

```rust
// lib.rs, relay_preconnect:
ActiveLeg::Relay => {
    transport::preconnect(&this.relay).await?;      // chat leg first — don't race it for a relay conn slot
    relay::leg_pool::warm().await;                  // dial one API leg and park it (unproven, 12s TTL)
    tokio::spawn(refresh_relay_apns_best_effort(this.apns.clone()));
    Ok(())
}
```

`warm()` parks without sending a request, so `reuse == None` ⇒ 12s TTL. The user
reaches the list refresh within 1–3s of foregrounding, so **the cold list refresh's
dial leaves the critical path entirely** — the single biggest win in the whole
change. Cost: one wasted relay conn slot for ≤12s if the user foregrounds and does
nothing.

### The numbers

| Value | Where | Buys | Costs |
|---|---|---|---|
| `TUNNEL_IDLE_TIMEOUT = 20s` (**unchanged**) | gateway, first head | a dialled-and-silent leg is still a leak; reclaim it | — |
| `TUNNEL_REUSE_IDLE_TIMEOUT = 60s` | gateway, subsequent heads; the value of `reuse.idle_ms` | "list appears → user taps in" (1–10s), most of "open chat → first send" (5–60s), a notification-banner bounce | one **non-reserved** relay conn slot per parked leg (`serve.rs:1091` — Api and Chat share a pool; `CHAT_CONN_RESERVE=4` only fences blob) + a gateway task + a few KB of `TransportState`, for ≤60s |
| client `ttl = min(idle_ms, 120s) − 10s = 50s` | client | **the client always closes first** | the 50–60s reuse window is given up |
| `UNPROVEN_LEG_TTL = 12s` | client, `warm()`ed legs | pre-dialling needs no negotiation at all | a wasted dial if the user idles 12s |
| `MAX_POOLED_LEGS = 3` | client | 3/6 hit rate on the open-chat fan | ≤3 parked slots per device |

Why not 90s: it would only buy "read the list for half a minute, then tap", while
multiplying relay-side residency 4.5×. The relay is a **third independently
versioned component** and has no reaper for data legs (only the control link has
`CONTROL_IDLE_TIMEOUT = 90s`) — **the gateway's idle timeout is the only thing that
reclaims them.**

**Honest statement:** `CLIENT_IDLE_MARGIN` is an *optimization*, not a correctness
mechanism. The gateway can still close a leg at an instant the client cannot know
(restart, relay flap, NAT). **The mechanism is the one-shot retry.** The margin only
saves retries — which is precisely why the id check and the drain rule are
non-negotiable prerequisites.

### Failure modes

| # | Scenario | Detection | Behaviour | User cost |
|---|---|---|---|---|
| 1 | Parked leg reclaimed between requests (gateway idle / relay drop / restart / NAT) | send error or EOF before first byte ⇒ `!response_seen && was_pooled` | discard, dial fresh, retry once (`Replayable::Yes`) | an RST round trip (usually <1 ms) + one normal dial |
| 2 | Parked leg becomes a blackhole | `POOLED_LEG_FIRST_BYTE_TIMEOUT = 4s` | discard, dial fresh, retry once | ≤4s + a dial. **The only new tail-latency mode**; the three backgrounding layers push its rate toward zero |
| 3 | App backgrounds → suspends | synchronous epoch bump on `.background`; dual-clock stamp | pool emptied; an in-flight request's leg is discarded on completion (stale epoch) | first call after foreground dials cold (= today) |
| 4 | Suspended mid-request | socket usually dead on resume → 4s/30s timeout | discard; no retry if any response byte was seen | ≤30s; the caller (list refresh / sync) already has a retry path |
| 5 | Leg dies mid-upload (JSON body, single chunk) | `sent_any && !response_seen` | retry per `Replayable` (all bodied routes are `Yes`) | one re-dial |
| 6 | Router hangs up on the request body (401/404 before any extractor reads it) | gateway `BodyDrain::Abandoned` | response still sent, **leg MustClose** | client gets its correct 401/404. **This is the live detonator**, masked today by the leg always dying |
| 7 | Non-2xx (`chat_lookup_message`'s 404 — the normal outbox-rebase result) | status check | **drain the body first**, then `Err`; leg poolable per `reuse` | none. Today it would poison the next request on a reused leg |
| 8 | Gateway sends `TunnelResponse::Error` | blanket rule | leg is dead ⇒ discard, no pool, no retry | one re-dial |
| 9 | Response carries the wrong `request_id` | new id check | `Desync` ⇒ kill the leg, **no retry** | fail-fast instead of feeding request N's JSON to request N+1 |
| 10 | Logout / re-pair to another gateway | `epoch` + `BindingKey` | old legs fail both gates and are dropped | none. **Critical:** `reuse` lives on the `PooledLeg` and dies with it — it is **not** a process-global capability memo, or a rollback to an old gateway would leave the client speculating into a 30s hang |
| 11 | 6–12-way concurrency (open-chat fan; ≤12 stores reconnecting on foreground) | `take()` → `None` | dial immediately (today's path, zero added latency); park on completion if there is room | none — never worse |
| 12 | Old app's empty-body straggler frame + new gateway | straggler drain (`request_id <= last_id`) | dropped, loop continues | none |
| 13 | 100 MiB blob transfer | `LegClass::Blob` → single-request path | byte-identical to today; no loop, no lifetime cap | none |

### Blob legs: never pooled

A 100 MiB transfer would block every API call behind it for minutes. The relay also
meters `Blob` under `register_background` (`serve.rs:1090`), so interactive requests
riding a background-class leg would be mis-metered and possibly throttled. And one
dial amortized over 100 MiB is noise. Enforced **server-side** by answering
`reuse: None` on any `LegClass::Blob` leg, not by client good behaviour. **Never add
a lifetime cap to a blob leg** — it would guillotine a legitimate 10-minute upload.

## Rollout

All four stages have landed, each as its own commit, in this order. Stages 0 and
1 stand alone: they fix bugs that existed before any of this, and neither depends
on the other.

**Stage 0 — the gateway.** `LegClass` threading + the request loop +
`LegState`/`BodyDrain` + the desync table + the straggler drain + the
`TunnelReuse` field. A no-op for every client shipped so far: it sends one
request and closes, and the loop exits on that EOF at once. Fixes the `tx.send`
detonator regardless.

**Stage 1 — client framing hygiene, still on one-shot legs.** `LegIo` +
request-id checking + the frame queue + non-2xx draining + empty-body
normalization + `TUNNEL_REQUEST_TIMEOUT` (there had been no per-request budget at
all). A `TunnelFrames` seam went in underneath so the framing rules could be
tested against a scripted gateway — no relay, no socket, no crypto — which is
also what Stage 2's pool tests use.

**Stage 2 — the pool, the barrier, `Replayable`.** Where the win starts, and the
only stage that needs both of the others.

**Stage 3 — the pre-dialed leg and the decoupled APNs POST.** Takes the cold list
refresh's dial off the critical path.

### Deviations from the design as written

- `PooledLeg` / `ApiLegPool` are **generic over the `TunnelFrames` seam**
  (defaulted to `NoiseFrames`), which the design did not call for. Without it the
  pool could only ever hold a leg backed by a real socket, and none of its rules —
  the TTLs, the epoch, the binding gate, the two clocks — would have been testable
  at all.
- `should_retry` is a **pure function**, lifted out of the request path. The
  at-least-once bargain is the fragile part of this change, not the plumbing
  around it, and it deserved to be readable and tested on its own.
- The APNs refresh is **skipped entirely when the token is unchanged**, which is
  the steady state. The design only required it to stop gating `transport::connect`;
  the skip is what makes a foreground (up to a dozen resident stores reconnecting)
  stop firing a dozen identical POSTs.

### Still owed: measurement

**Nothing here has been measured.** `MAX_POOLED_LEGS = 3` is a guess, and so is the
claim that a warm leg is usually there when the next call comes. The number that
settles both is the shape of the real call graph: how much of it is human-paced
serial (list appears → two seconds later a tap → sync), where reuse hits every
time, versus a same-millisecond fan, where it hits `K/N`.

Put a counter on `dial_tunnel_leg(LegClass::Api)` and one on `ApiLegPool::take`'s
hit rate, run a day of real usage, and let that decide whether `MAX_POOLED_LEGS`
should be 2 or 4. The staging was chosen so this can be answered after the fact
rather than guessed at up front — Stages 0 and 1 earn their keep either way,
because they fix bugs that predate all of this.

### Still owed: a device regression

CI does not cover `app/ios` beyond the Rust and web tiers plus the simulator Swift
suite, and none of that exercises a real relay. On a relay binding, by hand: list →
open a chat → sync → mark-read; background for 60s and for 30 minutes, then
foreground; kill the gateway mid-burst; re-pair while a request is in flight. And
run one pass against an **old gateway binary** — the skew path is the one thing
here that no test in the repo can reach.

## What this costs

- **A whole new bug class: framing desync.** A one-shot leg makes it *structurally
  impossible*. `LegState`/`BodyDrain` are the typed defence and the client's id check
  + drain rule are the other half — but "I made framing stateful and I believe I
  found every exit" is an **assertion, not a proof**, and its failures are silent and
  delayed. This is the design's biggest cost.
- **A new tail-latency mode**: a zombie leg costs ≤4s.
- **Relay residency**: up to 3 parked legs × 60s per device, on the **non-reserved**
  conn quota. Today the gateway holds nothing between calls. A self-hoster with
  `max_conns = 8` and three phones would have parked legs competing with chat-leg
  reconnects — which is exactly why `MAX_POOLED_LEGS` is 3 and the idle is 60s, not
  90s.
- **At-least-once semantics.** Today it is exactly-once-or-fail. Safety rests
  entirely on the *current fact* that every route here is idempotent; `Replayable`
  turns that into a compile-time obligation, but that is a guard rail, not a theorem.
- **`request_id` becomes load-bearing.** Today it is a constant, ignored, harmless.

## What this does not fix

- **The first API call of a process** still dials — unless `pool.warm()` (Stage 3)
  gets there first, and that only exists on a relay binding with the chat leg already
  up.
- **Blob transfers** still dial per transfer. Deliberate.
- **The number of REST calls.** The list still refetches on every appear. Reuse makes
  each call ~5× cheaper; it does not make it free. If "the list is slow" is the real
  pain, the bigger lever is serving it from the local `SessionIndex` and reconciling
  in the background.
- **The direct leg** — already pooled.
- **True concurrency beyond `MAX_POOLED_LEGS`** — constraint 1 forbids pipelining, so
  the excess callers dial their own (= today; not worse, not better).
- **`reconcileOutboxOnConnect`'s N-way point-lookup fan.** The pool stops it from
  being *worse*; the real answer is a batch endpoint, which is a separate change with
  its own partial-failure semantics.

## Rejected

- **Pipelining on one leg** — `ApiTunnelSession` is an ordered, stateful Noise
  transport; out-of-order decrypt is fatal. Concurrency comes from more legs.
- **A Ping/keepalive frame** — an unknown `type` tag kills the very leg it would keep
  alive, and the only thing it could buy is outliving the budget the server just
  declared. To widen the window, change `TUNNEL_REUSE_IDLE_TIMEOUT`.
- **REST multiplexed over the chat leg** (tempting, because `relay_preconnect`
  already dials and handshakes it). `write_chunked` is frame-atomic, the
  `FrameReassembler` is one byte buffer per transport, and the gateway's write side is
  a single `NoiseFrameSink` (`device_content.rs:261`) — a 200-row sync page
  (100–400 KB) would inject hundreds of ms of dead air into a streaming answer. Blobs
  could not ride it either, so you would end up maintaining **two** REST-over-relay
  implementations. And an old gateway **warn-and-skips** an unknown `Frame`
  (`device_content.rs:330-333`) — *silently* — so a new app would hang to timeout on
  every REST call. Skew safety is strictly worse. Would need its own ADR.
- **Persisting the capability** — the `PairedRecord` keychain JSON is a frozen upgrade
  contract, and it would buy nothing anyway: the first request of every process has to
  dial regardless.
