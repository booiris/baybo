# Mobile Blob Transfer

The mobile companion moves blob bytes (images, files, voice notes) over dedicated
relay data legs. This extends the existing gateway `BlobStore` side channel to a
NAT-traversed phone without putting bytes on the chat WebSocket.

Implemented pieces:

- chat-priority background bandwidth (`remote-host/crates/ratelimit`,
  `remote-host/crates/relay/src/bandwidth.rs`);
- `x-relay-leg-class` plumbing and connection-cap reserve
  (`remote-host/crates/{protocol,relay}`);
- the gateway blob responder for download/upload with `BlobStore::open_at`,
  per-blob size enforcement, and content-hash validation
  (`crates/gateway/src/channel/blob_content.rs`, `crates/store`, `crates/storage`);
- the shared sub-protocol (`crates/device-proto/src/blob.rs`);
- the mobile Rust/Tauri/TS client (`app/mobile/core/src/blob.rs`,
  `app/mobile/src-tauri/src/blob.rs`, `app/mobile/src/blob.ts`).

There is deliberately no background blob sweeper and no
`BlobStore::purge_older_than`: device-uploaded blobs are durable. The upload
path keeps the per-blob 100 MiB cap, but does not enforce a per-device aggregate
quota. A global disk ceiling or reference-tied lifetime sweep would need
additional `BlobStore` surface.

The mobile chat-view UI is now wired (`app/mobile/src/App.tsx` + `blob.ts`): an
`accept="image/*"` picker stages picked images and uploads them over a blob leg
(`blob_upload_bytes`, raw IPC body since iOS hands the webview `File` bytes, not a path),
the outgoing `Frame::Message` carries the resulting `WireAttachment`s, and inbound/restored
image attachments render via a download-to-cache + object-URL component (`blob_image`).

The shape in one line: a **dedicated relay leg per blob transfer** (separate TCP + Noise +
pump from chat), metered against the **same** per-tenant bandwidth budget as chat but as a
**background class** that leaves chat a reserved headroom — so a 100 MiB transfer never
slows interactive chat, the per-tenant bandwidth wall stays a single number, and the relay
stays content-blind.

## Goal

Let the mobile companion **download** attachments the agent produced and **upload**
attachments the user picked, while it reaches the NAT'd gateway only through the relay's
blind splice. Without this path it would receive `WireAttachment` references it
could not dereference.

Two hard requirements drove the design:

1. **Chat stays responsive during a bulk transfer.** A 100 MiB download must not add
   seconds of latency to a streaming answer. This is why the existing `/v1/blobs` HTTP
   side-channel was split off the chat WebSocket in the first place (see
   [`sidecars.md`](../../sidecars.md) §"Media side-channel").
2. **One per-tenant bandwidth wall.** Blob traffic must not get its *own* extra ceiling
   that widens a `remote_api_key`'s total relay footprint. Chat and blob share one budget;
   blob yields within it.

## Non-goals

- **Length/timing hiding from the relay.** Noise is not a length-hiding transport; the
  relay already observes per-frame ciphertext sizes for chat. Blob transfer does not
  regress this and does not try to fix it (an optional fixed-size-chunk padding is noted
  under open questions).
- **A relay that understands blobs.** The relay never learns `blob_id`, `read_token`,
  plaintext, device identity, or pull-vs-push. It learns one bit per leg — the traffic
  **class** — and copies opaque Noise frames blind, exactly as it does for chat.
- **Cross-session / multi-user read sharing.** Device pairing is single-user
  (see [`companion.md`](companion.md)); read authorization is the
  per-blob capability token, not a session ACL.

## Background

The relay (`remote-host/crates/relay/`) is a blind WebSocket splice with exactly five
routes (`remote-host/crates/protocol/src/relay.rs:18-22`): `/pair/host`, `/pair/join`,
`/control`, `/content/join/{relay_node_id}`, `/content/host/{relay_key}`. None carry blob
bytes. A content session rides one Noise-IK leg over the
`/content/join` ↔ `/content/host` splice; the gateway runs the Noise IK responder
(`crates/gateway/src/channel/device_content.rs`), authenticates the device by matching the
initiator static key to an approved device row, then runs the **same** `wire::Frame` chat
loop the TUI/web use.

On that loop the gateway emits `Frame::Attachment` carrying `WireAttachment{kind, blob_id,
mime_type, size, filename}` — a **reference only**. The `WireAttachment` doc
(`crates/wire/src/lib.rs:95-101`) is explicit: the bytes "never ride the WS — they live in
the gateway's `BlobStore` and are uploaded/fetched out-of-band via `POST/GET /v1/blobs/*`."
That out-of-band route is HTTP on the gateway — channel-token-authenticated for the TUI
and local sidecars, admin-token-authenticated for web chat, but **not** reachable by a
phone behind NAT. So the companion holds attachment references it cannot resolve.

## Contract (non-negotiable)

1. **Relay stays content-blind.** No `blob_id`, `read_token`, plaintext, `device_id`, or
   transfer direction reaches C. C sees: which route was dialed (= class), and the
   ciphertext sizes/timing it already sees for chat.
2. **One bandwidth wall per `remote_api_key`.** No second per-key ceiling. Blob shares the
   existing `(key)` and `(key, server)` buckets and yields to chat within them.
3. **Read is the capability token.** A device may fetch a blob iff it holds the blob's full
   token-bearing `blob_id`, which it only ever obtains over its own authenticated chat
   leg. Identical to `GET /v1/blobs/{id}` (`crates/gateway/src/channel/blobs.rs`).
4. **Session rows are untouched.** Staging and reclamation operate on blob/temp storage
   only, never on session rows (see [`CLAUDE.md`](../../../CLAUDE.md) §"Session data is core
   data").

## Decision: dedicated legs + chat-priority shared bandwidth

The decision space and why the alternatives lose:

- **In-band frames on the chat leg** (a `BlobChunk` `wire::Frame` multiplexed onto the chat
  Noise stream). Smallest surface, zero relay change, automatically E2E. But chat and blob
  share one TCP socket, one Noise session, and one outbound pump, so a big transfer
  head-of-line-blocks chat at the gateway pump and the kernel socket — fixable only with a
  two-lane priority pump **and** an application sliding window. Crucially, **the relay
  cannot prioritize in-band**: the chat+blob bytes are one opaque Noise stream, so C cannot
  tell a blob byte from a chat byte and cannot give chat a bandwidth headroom. In-band
  forces all protection onto the gateway/client.
- **Dedicated blob leg, shared bandwidth, equal priority** ("plain B"). A separate leg
  isolates TCP/Noise/pump, but the relay throttle meters by `(remote_api_key, server_id)`,
  **not by connection** (`remote-host/crates/relay/src/bandwidth.rs:8-17`). Both legs of a
  gateway resolve to the *same* bucket pair (`limiter_for` returns a shared handle;
  test `both_legs_of_a_session_share_one_bucket_pair`, `bandwidth.rs:175`). So a saturating
  blob transfer drains the shared bucket and chat is paced at the relay anyway — the
  dedicated leg buys nothing at the bandwidth layer.
- **Dedicated blob leg + separate bandwidth ceiling** ("B+ separate-class"). Gives blob its
  own bucket — but that **doubles the per-tenant wall** (chat 1 MiB/s + blob N MiB/s), and
  since the class is phone-authored, any phone can relabel chat traffic as blob to claim
  the larger ceiling and bypass the chat wall. Rejected for widening the cross-tenant wall
  and being abuse-prone.

**Chosen: dedicated blob leg + chat-priority background class on the *shared* bucket.** The
relay must distinguish blob from chat to give chat priority — which is *only* possible with
separate legs (in-band's opaque stream forbids it). So the dedicated leg is not an
optimization here, it is the **enabler** of chat-priority bandwidth. And because blob
shares chat's single bucket (just yields within it), the wall stays one number and the
class tag cannot be abused to exceed it (relabeling blob-as-chat only forfeits the device's
*own* chat priority — self-harm, never a cross-tenant gain).

## Architecture

### Leg topology and the class header

No new routes. The phone declares a leg's class on its existing `/content/join` dial via a
new header:

```
x-relay-leg-class: chat | blob        (RELAY_LEG_CLASS_HEADER; default chat when absent/unparseable)
```

The phone owns the join, so the phone authors the class. `content_join_handler`
(`remote-host/crates/relay/src/serve.rs:532`) reads it and stores `(server_id, class)` as
the value of `pending_content_legs`. It threads the class into the control signal,
and recovers it on the gateway-side host leg
(`content_host_handler`, `serve.rs:586,618`) so **both** spliced legs meter against the
class-matched limiter. The class picks `Interactive` vs `Background` reservation on the
**same** bucket pair — `limiter_for` is unchanged.

A phone-authored, relay-trusted class is safe here precisely because of the shared bucket
(§Decision): within a tenant it only chooses that tenant's own chat-vs-blob priority; it
cannot cross the per-key wall.

### Control signal

`ControlSignal::OpenDataLeg` gains a `class` field (or a sibling `OpenBlobLeg{relay_key}`):

```rust
ControlSignal::OpenDataLeg { relay_key: String, class: LegClass }   // LegClass = Chat | Blob
```

Flow (mirrors content): phone dials `/content/join/{relay_node_id}` with
`x-relay-leg-class: blob` → C mints `relay_key`, records `(relay_node_id, Blob)` in
`pending_content_legs`, calls `signal_open(relay_node_id, relay_key, Blob)` which pushes the
signal down the gateway's cap-exempt `/control` connection → gateway dials
`/content/host/{relay_key}` and runs the **blob sub-protocol** (not the chat loop) → C
splices by `relay_key` and meters both legs `Background`.

### The blob sub-protocol

After the identical Noise IK responder handshake (`device_content.rs:83-115`; device auth =
`lookup_approved_by_pubkey`), the leg runs a small request/response loop — **not**
`run_inbound_loop` / `wire::Frame`. Each logical message is Noise-sealed through the same
`write_chunked` / `FrameReassembler` chunking as chat
(`crates/device-proto/src/noise.rs`, `≤ NOISE_MAX_MESSAGE = 65535`). One transfer per leg,
so no per-message multiplexing is needed; `request_id` is kept only to correlate a resume.

```
phone → gw   BlobPull  { request_id, blob_id, offset }            // download / resume
gw → phone   BlobChunk { request_id, offset, data(≤64 KiB), last }
gw → phone   BlobDone  { request_id, total_size, sha256_hex } | BlobErr { reason }

phone → gw   BlobPush      { request_id, mime, size, sha256_hex, filename? }   // upload
gw → phone   BlobPushGrant { request_id, resume_offset } | BlobErr
phone → gw   BlobChunk     { request_id, offset, data, last }
gw → phone   BlobPushDone  { request_id, blob_id }
```

64 KiB chunks seal to ~one Noise message, comfortably under the relay's 128 KiB frame cap
(`serve.rs:61`).

### Bandwidth: chat-priority background class

The whole bandwidth change is a **class-aware reservation on the shared bucket** — no new
bucket, no new admission column, no doubled ceiling. `limiter_for` is unchanged: a
gateway's chat and blob legs resolve to the **same** `(key)` and `(key, server)` buckets
(one wall). Only the per-frame `throttle` call differs by class.

Add to `TokenBucket` (`remote-host/crates/ratelimit/src/lib.rs`):

```rust
/// Like `reserve`, but keep `headroom` tokens out of reach so a higher-priority
/// (interactive) caller on the same bucket always finds them. Background traffic
/// borrows down to `headroom` and no lower, so an interactive `reserve` of a frame
/// ≤ headroom is never paced, no matter how greedy the background stream.
pub fn reserve_background_at(&mut self, n: f64, headroom: f64, now: Instant) -> Duration {
    self.refill(now);
    let available = self.tokens - headroom;   // only tokens above the reserved floor
    self.tokens -= n;                          // background really spends n
    if available >= n {
        Duration::ZERO
    } else {
        // pay back up to the headroom floor (not 0), so the bucket never sits below it
        Duration::from_secs_f64((headroom - self.tokens).max(0.0) / self.refill_per_sec)
    }
}
```

`BandwidthLimiter` gains a class and a per-bucket headroom (derived from the bucket's
current capacity so it tracks an admission hot-reload):

```rust
pub enum LegClass { Interactive, Background }
const CHAT_HEADROOM_FRACTION: f64 = 0.25;   // reserve ~25% of the 1 s burst for chat

fn reserve(&self, nbytes: usize, class: LegClass) -> Duration {
    let n = nbytes as f64;
    match class {
        LegClass::Interactive => self.key.reserve(n).max(self.server.reserve(n)),
        LegClass::Background => {
            let hk = self.key_capacity()    * CHAT_HEADROOM_FRACTION;   // ≈256 KiB at default
            let hs = self.server_capacity() * CHAT_HEADROOM_FRACTION;
            self.key.reserve_background(n, hk).max(self.server.reserve_background(n, hs))
        }
    }
}
```

Behaviour: chat idle → background consumes everything above the floor and runs at the full
refill rate `R` (`RELAY_BYTES_PER_SEC = 1 MiB/s`, `bandwidth.rs:42`), bucket hovering at
`headroom`; chat sends a frame `s ≤ headroom` → its `reserve` finds ≥ `headroom` tokens and
is paced **zero**. The band `[negative-burst, headroom]` is chat-exclusive. Work-conserving
(blob uses all spare) and chat-priority (blob yields the floor), within one wall.

The relay call sites: `content_*` handlers throttle `Interactive`, blob legs throttle
`Background`; `pump_ws` takes the class.

### Per-layer isolation proof

| Layer | Why chat cannot contend with blob |
|---|---|
| TCP | Chat leg and blob leg are **separate WS/TCP connections**; blob bufferbloat sits on its own socket. |
| Noise | Separate Noise sessions; no shared sealer or transport mutex. |
| Gateway pump | Separate per-leg pump tasks; **no shared outbound FIFO**. |
| Relay bandwidth | Shared bucket, but blob is `Background` and leaves `headroom`, so chat always finds tokens and is paced ~0. |
| Relay pump | Each leg has its own `pump_ws`; the chat leg's pump is never blocked by the blob leg's. |

Because isolation holds at every layer, **no application sliding window is needed** (unlike
in-band, whose shared FIFO and shared Noise stream force one).

### Concurrency and connection-cap governance

Blob legs run **concurrently — one dedicated leg per transfer — and are NOT deduped.**
(An earlier revision deduped them by `device_id` to a single warm leg per device, but that
made two concurrent transfers for the same device abort each other; the dedup was dropped.)
Only the **chat** content leg dedups (`device_leg_registry` + `LegDedup` — a fresh chat leg
aborts a stale predecessor). Concurrent blob transfers are bounded instead by the relay's
per-key connection cap.

The fallback per-key connection cap is `DEFAULT_MAX_CONNS_PER_KEY = 200`
(`conns.rs`, `register`; `/control` uses cap-exempt `register_unchecked`). Docker
Compose may set a different fallback through `MAX_CONNS_PER_REMOTE_API_KEY`.
Blob host legs go through `register_background`, capped at
`cap - CHAT_CONN_RESERVE`, so a chat leg can always reconnect into the reserved
headroom. Under the shared-`remote_api_key` model, that cap is shared by all
devices using the key; a gateway-side per-device sub-cap is a possible future
refinement.

Other mitigations (see Adversarial #4):
- **Pre-Noise reaping:** the blob responder reuses `HANDSHAKE_TIMEOUT` (10 s,
  `device_content.rs:44`); a stalled handshake closes the gateway leg, which breaks the
  splice and frees the relay cap slot. A per-device pending-blob-leg bound (so a dial flood
  can't accumulate to the cap within the timeout window) is a possible refinement.
- **Half-open reaping:** add a read-idle timeout (mirroring `CONTROL_IDLE_TIMEOUT`,
  `serve.rs:52`) or a keepalive ping to content/blob **host** legs on the relay, so a phone
  that vanishes without a FIN doesn't pin a cap slot until OS TCP keepalive.

### Read authorization (the capability token)

Read scope is the `read_token` already baked into every `blob_id`, **not** a session ACL or
an offered-set. `blob_id = "sha256:<hex>.<read_token>"`, where `read_token` is a 128-bit
unguessable value minted per put. `crates/storage/src/libsql/blob.rs` enforces the token
in `stat()`; `get()`, `open()`, and `open_at()` are token-safe because they call `stat()`
before resolving the content-addressed path. The on-disk path is keyed by the hex digest
alone (`blob_path`), and `split_id` never compares the token — **any read path that
bypasses `stat()` has zero token enforcement.**

A device only ever learns a full token-bearing `blob_id` over its **own authenticated chat
leg** (the agent Noise-seals a `Frame::Attachment` to it). It cannot guess another blob's
token, so it cannot pull a blob it was never sent — identical to how the existing
`GET /v1/blobs/{id}` authorizes on the token alone (`blobs.rs:238-268`, which doesn't even
take an `authed`). Defense-in-depth: the blob leg also requires a Noise-IK-approved device
row to exist at all.

**Critical implementation constraint.** `BlobStore::open_at(blob_id, offset)` **must
re-enter `stat()` before opening `blob_path(hex)` and seeking**. The obvious
"skip the DB hit, open by hex and seek" shortcut would drop the only token gate and let a
device holding just the bare hex pull from `offset`. `open()` is implemented as
`open_at(_, 0)`. `stat()` treats a NULL `read_token` as fully open for backward
compatibility, so legacy NULL-token blobs remain pullable with the bare hex.

### Upload gate and limits

The upload path flips the gateway's receive-only posture (`authorize_upload` currently
rejects `AuthedClient::Device`, `blobs.rs`) only behind these bounds (Adversarial #5):

- **Gate:** the Noise-IK approved-device check already authenticates the leg; no extra
  token needed.
- **Per-blob size cap:** `put_stream`'s incremental `max_bytes` caps a single blob at
  100 MiB (`MAX_BLOB_BYTES`). There is no per-device aggregate quota on this leg, and
  device-uploaded blobs are durable (no LRU/age-based delete).
- **Integrity:** `BlobDone.sha256_hex` / the claimed push hash must equal the **hex prefix**
  of the content-addressed `blob_id` (split on `.`), not the whole id.

**Deferred (needs extra `BlobStore` surface):** a global disk ceiling (for example a
`total_bytes` method) and a reference-tied-lifetime sweep of unreferenced device uploads.

## Mobile client

The companion is a Tauri app (`app/mobile/`): a Rust core that runs the Noise IK
**initiator**, and a TS/React UI.

- **Rust core:** a blob-leg client that dials `/content/join` with `x-relay-leg-class: blob`,
  runs the Noise IK initiator and the blob sub-protocol, writes `BlobChunk`s to a temp file
  at `offset`, verifies `sha256` against the requested `blob_id` hex on `BlobDone`, and
  resumes by re-`BlobPull`ing from `temp_file_len` after a drop.
- **TS/React:** `useBlobDownload` / `useBlobUpload` hooks; `MessageAttachment` renders an
  image/file once downloaded (spinner + retry); send-with-attachment uploads first, then
  embeds the returned `blob_id` in the outgoing `Message`.

## B+ vs in-band: when to pick which

- **This design (dedicated leg + chat-priority background class)** when chat must stay
  responsive during bulk (up to 100 MiB), the per-tenant wall must stay a single number,
  and a relay change is acceptable. The relay change is small: one phone-authored header
  bit + the `reserve_background` reservation; no new routes, no new buckets.
- **In-band two-lane + sliding window** when the relay is frozen (no class signal can be
  added) or blobs are small and frequent and a second connection slot is unwanted. Accept
  that blob shares the chat leg's Noise/TCP/pump and isolation rests entirely on a software
  scheduler + window (weaker than four physical layers), and that the relay cannot
  bandwidth-prioritize an opaque in-band stream.

## Security / threat model: what C learns

C learns exactly one new bit per leg — the **class** — phone-authored and copied blind,
plus the ciphertext size/timing/direction it can already infer from chat frame patterns. It
never learns `blob_id`, `read_token`, plaintext, `device_id`, or pull-vs-push (all inside
the Noise session, after the splice). The class is *not* C-authored — this is what keeps C
blind (the rejected separate-class variant had to feed `blob_id`/scope to C, which holds
only `relay_node_id` at join time). Honest residual: the explicit class label turns "a bulk
transfer is probably happening" (already inferable) into a labeled fact, and lets an
operator correlate blob-leg lifetimes with transfer events. This is the consciously
accepted cost; content, identity, and access control remain end-to-end protected.

The class tag **cannot be abused** to exceed the wall or steal bandwidth: there is one
shared bucket, so mislabeling blob-as-chat only forfeits the device's own chat priority
(self-harm), and the per-key wall bounds the tenant regardless.

## Out of scope / open questions

- **Tiny-media inline push** (gateway pushes a thumbnail/voice note below a small threshold
  without a pull RTT) — possible future optimization; must stay capability-scoped.
- **Fixed-size chunk padding** to collapse the per-chunk size leak to chunk-count
  granularity — optional, deferred; full timing/volume hiding is a non-goal.
- **Upload resume across a new session** (fresh `request_id` restarts from 0) — acceptable,
  or add a content-prefix handshake to rebind staging.
- **iOS sandbox temp path** for partial-download resume across app backgrounding — confirm
  the path survives suspension and is purgeable.
- **Per-device blob concurrency** under a shared `remote_api_key` — relay caps the key
  globally today; a gateway-side per-device sub-cap may be useful once real usage data
  exists.

## Related

- [`sidecars.md`](../../sidecars.md) — the existing `/v1/blobs/*` media side-channel and
  `BlobStore` model this extends to NAT'd clients.
- [`storage.md`](../storage.md) — libsql `BlobStore`, `read_token` capability.
- [`pairing.md`](../pairing.md) / [`gateway.md`](../gateway.md) —
  device pairing, the relay content path, and the gateway channel loop.
