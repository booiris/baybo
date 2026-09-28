# Direct Carriers for Relay Bindings

**Status: proposed, not implemented.** It is delivered as two stacked PRs:

- **PR1: protocol, C and gateway.**
  - Wire types and the address policy (`remote-host/crates/protocol`).
  - Candidate sealing and punch authentication (`crates/device-proto`).
  - A shared QUIC carrier crate (`crates/carrier`).
  - `POST /direct/{relay_node_id}` and the per-punch UDP rendezvous on C.
  - The gateway's carrier runtime.

  PR1 is inert until an app speaks it and is covered by gateway↔C e2e tests. C deploys after PR1 merges.
- **PR2: the app.** The iOS ffi prober, carriers and idle chat rotation; the Swift path monitor and Settings row; the netns NAT matrix.

Paths cited as existing, usually with a line number, are on master. Everything the design introduces is planned and is listed in the *Delivery plan*; a change to code that already exists on master is marked (PR1) or (PR2).

A paired phone (P) reaches its gateway (A) through the operator's blind WSS relay (C), even when both sit on the same Wi-Fi. This design keeps that relay as the baseline that always works, and adds **direct carriers**:

- QUIC over UDP, on a LAN address, an IPv6 address, a public IPv4 address or a hole-punched IPv4 mapping;
- TCP, opt-in on the gateway.

P and A find a direct carrier through an ICE-lite candidate exchange signalled through C. The candidate lists are sealed end to end, with keys derived from the static keys the two endpoints learned at pairing. Every leg dials the relay first. A background probe then moves new API and blob legs onto a direct carrier once one is found, and the chat leg follows only when no turn is in flight. Every carrier session opens with a `DirectOpen` preface and then runs the same Noise IK handshake as a relay leg, so C's position for content is unchanged. The change is in metadata: C now observes IPv4 NAT mappings and learns that a direct attempt happened.

> The relay path, Noise IK and C's threat model are covered in
> [`relay-push-security.md`](relay-push-security.md). Pairing, which stays relay-only, is covered in
> [`pairing-security.md`](pairing-security.md). The leg pool and the `.background` barrier are in
> [`relay-api-tunnel.md`](relay-api-tunnel.md). The chat supervisor is in
> [`app/ios/docs/connection.md`](../../../app/ios/docs/connection.md). The architecture reference is
> [`companion.md`](companion.md).

## Naming: "direct" is scoped

**Direct mode** is the existing typed-URL + admin-token HTTPS path: `ActiveLeg::Direct` (`app/ios/ffi/src/binding.rs:18`), `app/ios/ffi/src/direct/`, and the "direct mode" of [`relay-push-security.md`](relay-push-security.md#direct-mode-push-web-identity). That path has no pairing, no relay (C's push role still delivers its notifications) and no Noise, and this document does not touch it. The other mobile docs call its connection "the direct transport" and "the direct leg"; this document does not use those phrases.

Everything this document calls *direct* belongs to a **relay binding**, meaning a device paired through C:

- The wire and config names keep the `direct` prefix: `/direct/{relay_node_id}`, `ControlSignal::DirectOffer`, `gateway.direct_udp`, `gateway.direct_tcp`.
- The code lives under `carrier` names (`crates/carrier`, `crates/gateway/src/channel/carrier/`, `app/ios/ffi/src/relay/carrier/`), so it never sits next to `app/ios/ffi/src/direct/`.
- In prose, a leg on a direct carrier is a **carrier leg**, and its authenticated stream or connection is a **carrier session**.

## Goals

1. Automatic direct connectivity between P and A with **zero router configuration**, so that C leaves the data path whenever the network allows.
2. **Relay first.** No dial ever waits on direct discovery. The user sees the relay's latency at worst, never a stall caused by probing.
3. Content security is unchanged: **every carrier session is a Noise IK session between the paired statics.**
4. C learns as little as possible. Host candidates (LAN addresses, IPv6 addresses, a public IPv4 interface address) are opaque to C. C sees only the IPv4 mappings it observes itself.
5. Behaviour on each NAT class is pinned by an automated matrix.

## Non-goals

- A UDP relay (TURN) at C. The WSS relay already fills that role.
- Symmetric-NAT port prediction. Combinations where the gateway side has an address-and-port-dependent (symmetric) mapping, or where a symmetric side meets a port-restricted peer, stay on the relay.
- NAT64 synthesis. On an IPv6-only path without CLAT, P's IPv4 tiers are `not_offered`.
- Direct pairing. **Pairing is relay-only**, as [`pairing-security.md`](pairing-security.md) states. No direct or LAN pairing route exists.
- Remote access from the browser (`app/web`).
- Direct mode (see *Naming*).

## Terms

| Term | Meaning |
|---|---|
| **A / C / P** | The gateway / the operator's remote host / the phone, as in [`companion.md`](companion.md#roles). |
| **Carrier** | The transport that legs ride on. The **relay carrier** is C's WSS splice (`/content/join` ↔ `/content/host`). A **direct carrier** is one QUIC connection between P and A, or one proven TCP address. |
| **Candidate** | An address at which a side can be reached. A **host candidate** is an interface address that the side reports about itself: private IPv4, IPv6 ULA, IPv6 GUA, or a public IPv4 address on an interface (a host with no NAT). A **server-reflexive (srflx) candidate** is a side's public IPv4 mapping as observed by C's UDP rendezvous. |
| **Carrier session** | One authenticated leg on a direct carrier: a QUIC bidirectional stream, or a TCP connection, opened with `DirectOpen` and then Noise IK. It runs exactly the relay leg's content responder (Chat) or API tunnel (Api/Blob). |
| **Probe** | One background attempt by P to find a direct carrier. It consists of one `POST /direct`, a candidate exchange, an optional punch, concurrent QUIC connects, a TCP fallback, and a proof leg. |
| **Punch** | The per-probe state at C and A, named by a `PunchId` that C mints. It covers UDP registration, the `Peer` exchange and the punch datagrams. P's punch datagrams carry a tag only P and A can compute (**authenticated punches**). |
| **Upgrade** | New Api/Blob legs dial on the live direct carrier instead of the relay. |
| **Rotation** | The supervisor moves the live chat leg onto the direct carrier. This happens **only when the chat leg is idle.** |
| **Downgrade** | P retires the carrier. Its legs end, and new legs dial the relay. |

## Architecture

```
                 ┌──────────────────────── C (remote-host) ──────────────────────────┐
                 │ WSS relay: /pair/*  /control  /content/*      POST /direct/{node} │
                 │ /control WS:  C→A DirectOffer   A→C DirectAnswer | DirectDeclined │
                 │ UDP rendezvous (IPv4): per-punch Register → Registered | Peer     │
                 └───▲────────────────────▲─────────────────────────────▲────────────┘
       relay legs    │  offer / answer    │ sealed candidate sets       │ Register (UDP)
       (baseline)    │  (HTTPS)           │ (opaque to C)               │ each side, per punch
                 ┌───┴─────────────┐                                ┌───┴─────────────┐
                 │ P  phone        │ ═══ QUIC / UDP ═══════════════ │ A  gateway      │
                 │ one UDP socket  │   LAN v4 · ULA · v6 GUA ·      │ one UDP socket  │
                 │ per family      │   public v4 · v4 srflx         │ per family      │
                 │                 │ ─── TCP (only if A opts in) ── │ TCP listeners   │
                 └─────────────────┘                                └─────────────────┘
  every carrier session:  DirectOpen{token, class} ▸ Noise IK (pairing statics) ▸
                          run_content_session (Chat) | run_tunnel_session (Api, Blob)
```

The ranking below has two uses. It sets the order in which a probe prefers finished attempts, and it gives the display label for each carrier:

| Rank | Carrier (`CarrierKind`) | Reached via | Needs C's UDP rendezvous |
|---|---|---|---|
| 1 | `Lan` | QUIC to a private IPv4 or ULA host candidate | no |
| 2 | `Ipv6` | QUIC to a GUA host candidate; both stateful firewalls are opened by outbound traffic | no |
| 3 | `Ipv4` | QUIC to a public IPv4 host candidate | no |
| 4 | `Ipv4Punched` | QUIC to A's srflx mapping after a punch | yes |
| 5 | `Tcp` | TCP to a host candidate or to one of A's `advertised_addresses` | no |
| — | `Relay` | WSS via C | — |

**What C sees**, relative to [`relay-push-security.md`](relay-push-security.md#protected-assets):

| Item | Relay only | With direct carriers |
|---|---|---|
| Source IPs of P and A on HTTPS/WSS | yes | yes (unchanged) |
| P's and A's IPv4 UDP mapping (ip:port) | no | **yes**, observed by C itself during a punch |
| That a direct attempt happened, and its timing and outcome at C | no | **yes** |
| Host candidates and the direct token | — | **no**: they are sealed |
| A's QUIC certificate | — | not sent to C; C can fetch it by naming its own address as P's srflx (see *Security*), which grants nothing |
| How many candidates each side has, and whether A offers TCP | — | **no**: each sealed message kind has one fixed plaintext length |
| Lengths and timing of traffic on a direct carrier | — | **no**: the carrier bypasses C |

## Candidates

### Address policy: one home

All address classification lives in one type, `AddressPolicy` in `remote-host/crates/protocol/src/relay.rs` (PR1). A, C and P all call it. Nothing else re-derives an address class; the config validator does not either (see *Config*).

| Class | IPv4 | IPv6 |
|---|---|---|
| `Lan` | 10/8, 172.16/12, 192.168/16, 100.64/10, 169.254/16 | fc00::/7 (ULA) |
| `Public` | Anything not excluded below | 2000::/3, minus the exclusions |
| excluded | 0/8, 127/8, 192.0.0/24, 192.88.99/24, 198.18/15, documentation ranges, multicast, ≥240/4, broadcast | ::/128, ::1, fe80::/10 (it needs a scope id), multicast, 2001:db8::/32, 2001:2::/48, 2001::/32 (Teredo), 2002::/16 (6to4) |

IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are canonicalised to IPv4 before they are classified.

The protocol crate also owns the two derived questions that would otherwise be answered at each call site:

- `AddressPolicy::source_key(ip)` is the key of every per-source cap: the IPv4 address, or the IPv6 /64.
- `UdpRendezvous::resolve_public_v4(&policy)` resolves a rendezvous address with an A-record-only lookup and requires every result to be `Public` IPv4. A and P both call it.

Callers obtain the policy from `AddressPolicy::active()`. When the protocol crate is built with its `test-support` feature, `active()` returns `AddressPolicy::for_tests()`, which also classifies 198.18/15, 2001:2::/48 and 127/8 as `Public`, and `::1` as `Lan`. A release build does not enable the feature and cannot select that policy. The in-process e2e tests and the netns matrix build with it.

### Sockets

Each side holds **one UDP socket per address family** while its carrier machinery is active:

- **A** binds `gateway.direct_udp.ipv4_bind` (default `0.0.0.0:0`) and `ipv6_bind` (default `[::]:0`, with `IPV6_V6ONLY` set) once per binding runtime. It rebinds only on a persistent socket error. The port is ephemeral but stable for the life of the runtime.
- **P** binds `0.0.0.0:0` and `[::]:0` when a probe starts. The carrier the probe wins keeps them, and they close with it; a probe that finds nothing closes them. A P socket therefore never outlives the carrier it serves.

On each side, the UDP rendezvous registration, the punch datagrams and QUIC all share that one socket. This is required, not a convenience: the IPv4 mapping that C observes is the mapping QUIC uses. `crates/carrier` (PR1) wraps the socket in a `DemuxSocket` that implements `quinn::AsyncUdpSocket`:

- The socket is built on `quinn_udp::UdpSocketState`, so replies leave from the local address the peer targeted (`IP_PKTINFO` / `IPV6_RECVPKTINFO`). This matters on hosts with several IPv6 addresses.
- On Linux quinn-udp enables UDP GRO, so one receive buffer can hold several datagrams of one flow. The demux splits every buffer into `stride`-sized segments and classifies each segment on its own. Probe segments are decoded one at a time; QUIC segments are handed to quinn with their stride kept.
- A segment whose first byte has `0x80` or `0x40` set goes to quinn. Everything else is a probe datagram (see *Rendezvous and punch datagrams*).
- Probe datagrams are sent with `UdpSocketState::try_send`, so a send error reaches the caller. QUIC transmits go through `send`, which logs send errors and drops them. P relies on this to detect a refused Local Network permission (see *The prober*).
- Every endpoint sets `EndpointConfig::grease_quic_bit(false)`, so it never advertises the `grease_quic_bit` transport parameter (RFC 9287), and its peer therefore never sends it a packet with the fixed bit clear. Each side's own setting is what keeps its own demux unambiguous.

Neither side needs an inbound firewall or port-forward rule: **every side sends to a candidate before it expects to receive from it.**

### Gathering

**A** enumerates its interface addresses once per accepted offer. It does so only while at least one direct socket or listener is bound, so a gateway with direct carriers disabled never walks its interfaces.

- It reads addresses through netlink `RTM_GETADDR` on Linux, and through `getifaddrs` plus `SIOCGIFAFLAG_IN6` on macOS. It skips IPv6 addresses flagged temporary, deprecated, tentative or duplicate (DAD failed).
- It keeps addresses on UP+RUNNING interfaces whose class is `Lan` or `Public`.
- It skips interfaces whose names start with one of `VIRTUAL_INTERFACE_PREFIXES`: container bridges and veths (`docker`, `br-`, `veth`, `virbr`, `cni`, `lxc`) and VPN tunnels (`tailscale`, `wg`, `tun`, `utun`, `zt`).
- It keeps at most one GUA per /64 and at most `MAX_GATEWAY_GUAS` GUAs, `MAX_GATEWAY_ULAS` ULAs and `MAX_GATEWAY_IPV4_HOSTS` IPv4 addresses, ranked ULA/private < GUA < public IPv4.
- Each address is paired with the port of that family's stable socket.
- TCP candidates, present only with `gateway.direct_tcp`, are the addresses covered by a bound listener plus the configured `advertised_addresses`. They are capped at `MAX_TCP_CANDIDATES`.

**P** gathers from the primary interface of the currently satisfied `NWPath`:

- It reports **all** non-deprecated GUAs, up to `MAX_UDP_HOST_CANDIDATES`. iOS picks a temporary address as the outbound source, so P cannot know in advance which one it will use.
- It reports `Lan` IPv4 addresses and ULAs **only from a Wi-Fi or wired interface.** A private address on `pdp_ip*` (cellular CGNAT) can never be reached by a peer, and reporting it would make A spray datagrams at unrelated hosts on its own LAN.

**What P dials.**

- P attempts A's `Lan`-class candidates of a family only when its primary interface is Wi-Fi or wired and has a `Lan`-class address of that family. Otherwise the LAN tier is `not_offered`. On cellular, A's private addresses would route into the mobile carrier's network; on a foreign Wi-Fi that reuses A's subnet, they would reach unrelated hosts.
- P de-duplicates target addresses, for example A's srflx when it equals A's public IPv4 host candidate.
- When P's path has no IPv4 route (an IPv6-only network without CLAT), P's IPv4 tiers are `not_offered`.

## Candidate sealing

**Keys.** Both sides compute the keys from the Noise IK static keypairs they already hold (`crates/device-proto/src/noise.rs`):

```
shared  = X25519(own_static_secret, peer_static_public)       # reject all-zero ⇒ fail closed
prk     = HKDF-SHA256-Extract(salt = "baybo/direct/static-dh/v1", ikm = shared)
k_p2a   = HKDF-Expand(prk, "baybo/direct/seal/device-to-gateway/v1", 32)
k_a2p   = HKDF-Expand(prk, "baybo/direct/seal/gateway-to-device/v1", 32)
k_punch = HKDF-Expand(prk, "baybo/direct/punch/device-to-gateway/v1", 32)
```

`CandidateSealer::derive(role, own_secret, peer_public)` in `crates/device-proto/src/candidates.rs` (PR1) returns `{ seal: key for own direction, open: key for peer direction, punch: k_punch }`. The key properties are:

- The sealing keys are directional: a sealed set can never be reflected back to its sender as if the peer had sent it.
- The shared secret and the three keys are held in `Zeroizing`. The PRK exists only inside a transient `Hkdf` value, which hkdf 0.12 does not wipe, and that value is dropped right after the expands. `StaticKeypair::secret()` (`crates/device-proto/src/noise.rs:53`) returns a plain copy, and each call site wraps that copy in `Zeroizing` immediately.
- A holds one sealer per binding runtime. P derives one per probe and drops it afterwards.
- The keys have **no forward secrecy**. They protect addresses and a per-runtime token, not content, and anyone who compromises a static secret can already impersonate that endpoint.

**AEAD.** Sealing uses **XChaCha20-Poly1305** (`chacha20poly1305 0.10`, already a `device-proto` dependency) with a **fresh random 24-byte nonce** per seal.

- The key lives as long as the binding, and both a phone and a restarting gateway process seal under it. Random 192-bit nonces need no persisted counter and make a collision negligible.
- Associated data binds the context:
  - offer: `"baybo/direct/offer/v1" ‖ u64be(len) ‖ relay_node_id`
  - answer: `"baybo/direct/answer/v1" ‖ u64be(len) ‖ relay_node_id ‖ punch_id (16 bytes)`

**Plaintext.** The plaintext is `u16be(len) ‖ msgpack(body) ‖ zero padding` to **one fixed length per message kind**: `SEALED_OFFER_PLAINTEXT_LEN` for offers and `SEALED_ANSWER_PLAINTEXT_LEN` for answers.

- Each length holds the largest body the caps allow (every candidate IPv6, and a full `tcp` list), so the ciphertext length reveals neither the candidate count nor whether A offers TCP.
- `seal` fails if a body does not fit, and `open` rejects a plaintext of any other length.
- Fixed-size byte fields are encoded with `serde_bytes`, so their encoded length does not depend on their values.
- The plaintext buffer is `Zeroizing`, because an answer carries the direct token, and the token is decoded straight into `DirectToken`.

```rust
// crates/device-proto/src/candidates.rs (PR1)
pub const CANDIDATE_SET_VERSION: u8 = 1;

/// P → A, sealed under k_p2a.
#[derive(Serialize, Deserialize)]
pub struct DeviceOffer {
    pub v: u8,
    pub issued_at_ms: u64,           // P's wall clock, unix ms
    pub offer_id: OfferId,           // 16 CSPRNG bytes (serde_bytes): freshness challenge, replay key, punch-tag input
    pub udp: Vec<SocketAddr>,        // P's host candidates on its probe sockets
}

/// A → P, sealed under k_a2p.
#[derive(Serialize, Deserialize)]
pub struct GatewayAnswer {
    pub v: u8,
    pub issued_at_ms: u64,           // informational: skew logging only
    pub offer_id: OfferId,           // echo of DeviceOffer::offer_id
    pub token: DirectToken,          // DirectOpen gate; minted per binding runtime
    pub quic_cert_sha256: CertHash,  // 32 bytes (serde_bytes): P pins A's per-process QUIC certificate
    pub udp: Vec<SocketAddr>,        // A's host candidates on its stable sockets
    #[serde(default)]
    pub tcp: Vec<SocketAddr>,        // empty unless gateway.direct_tcp
}
```

**Freshness and replay.** C can store and replay anything it forwards, so both directions are protected:

- **A** accepts an offer only when all of these hold:
  - `|now − issued_at_ms| ≤ OFFER_MAX_AGE`;
  - `issued_at_ms` is not earlier than the runtime's `started_at_ms`, so no runtime accepts an offer issued before it existed, and a restart, which empties the replay cache, does not reopen replay;
  - its `offer_id` is not in the runtime's replay cache. The cache holds up to `MAX_REPLAY_ENTRIES` ids for `2 × OFFER_MAX_AGE`. When it is full, A declines the new offer (`declined:over_cap`) instead of evicting an unexpired id.

  A declined offer gets no punch. A P whose clock lags A's loses at most `OFFER_MAX_AGE` of probes after a gateway restart, which is one backoff step.
- **P** accepts an answer only when it echoes the `offer_id` of P's own in-flight offer. This is a challenge-response and needs no clock, so a stale answer cannot be replayed to P.

**Fail closed.** The following reject the **whole** set and end the probe for the side that detected it. A replies `DirectDeclined`; P stays on the relay and records a failure:

- an AEAD failure;
- a version mismatch;
- a plaintext of the wrong length, or a malformed body;
- a count over its cap;
- a stale or replayed offer.

An *authenticated* entry whose address class is excluded is dropped individually and logged. It came from the paired peer, so it signals a bug, not an attack.

## Signalling

### Capability gating

- **A's hello.** (PR1) `ControlHello` gains `direct: Option<DirectCapability>`. A sends it only when its carrier runtime has at least one socket or listener bound. `udp: true` means A has an IPv4 UDP socket and will register on demand.
- **How C decodes it.** C closes control on an unparseable hello (`remote-host/crates/relay/src/serve.rs:700-715`), so the capability field is decoded **leniently**: any decode failure yields `None` and is logged once. A malformed capability must not cost the gateway its relay.
- **Unknown versions.** An unknown `version` is treated as not direct-capable, and control stays up.
- **What C sends.** C sends `DirectOffer` only to a control connection whose hello carried a supported capability. An old gateway therefore never receives it. If it ever did, it would warn and skip the frame (`crates/gateway/src/relay/mod.rs:239-247`).

### Sequence

1. **P posts its offer.** P binds its probe sockets, gathers its host candidates, seals a `DeviceOffer`, and sends `POST /direct/{relay_node_id}` with the `x-remote-api-key` header and a `DirectOfferRequest` body.
2. **C admits and routes.** C admits the request the same way it admits every relay route:
   - the per-IP token bucket (`remote-host/crates/edge/src/ip_limit.rs`), then
   - `require_admitted` (`remote-host/crates/relay/src/serve.rs:491`).

   It then looks up the node's control entry. Each of the following produces the same opaque `404 no direct route`: no entry, an owner-key mismatch, or no capability. Next come the per-(node, client IP) rate, the per-node ceiling and the in-flight cap, each answering `429` with `Retry-After`.
3. **C mints the punch.** C mints a `PunchId`. If `UDP_PUBLIC_ADDR` is configured and the capability has `udp`, C also mints two `RendezvousTicket`s, one per role. C queues this signal on the node's control channel (`503` with `Retry-After` if the channel is full):

   `ControlSignal::DirectOffer { punch_id, offer, register }`

   `register` carries the rendezvous address and the gateway-role ticket.
4. **A handles the offer.** A opens the offer and runs the freshness and replay checks. If A already holds `MAX_INFLIGHT_PUNCHES_PER_NODE` live punches, the new offer supersedes the oldest: only P can seal an offer, and P runs one probe at a time, so a new genuine offer means P has abandoned the old one. A then creates the punch's **allowed-IP set** from P's host candidate IPs. It gathers its own candidates and seals a `GatewayAnswer` bound to `punch_id`. It then replies on the control WebSocket with:

   `ControlReport::DirectAnswer { punch_id, answer }`

   On any rejection it replies `DirectDeclined { punch_id }` and does nothing else.
5. **A punches and registers.** A sends `PUNCH_BURST` punch datagrams to every P host candidate, from each of its own host addresses in that family. It sends the highest-ranked pairs first and stops at `MAX_PUNCH_DATAGRAMS_PER_OFFER`. If `register` is present and A has an IPv4 socket, A resolves the rendezvous address with `resolve_public_v4`; if that fails, A skips registration. Otherwise it sends `Register{punch_id, Gateway, ticket}` from its IPv4 socket every `REGISTER_RETRY_INTERVAL` until it holds a `Peer` or `PEER_WAIT` elapses. A `Registered` reply confirms the ticket and does not end the loop.
6. **C answers P's POST.** C waits up to `DIRECT_ANSWER_TIMEOUT` for the report:
   - `DirectAnswer` → `200 DirectOfferResponse { punch_id, answer, rendezvous }`, where `rendezvous` carries the device-role ticket, with `Cache-Control: no-store`;
   - `DirectDeclined` → `404`;
   - no report within the timeout, or control gone → `504`.
7. **P checks the answer and starts connecting.** P opens the answer and checks the `offer_id` echo. To each A host candidate it may dial (see *What P dials*), P sends an authenticated punch and then starts a QUIC connect; the rest of that burst follows at `PUNCH_INTERVAL`. An Initial that overtakes its punch is ignored by A and retransmitted by quinn.
8. **P registers.** If `rendezvous` is present, P resolves it with `resolve_public_v4`. If that fails, the `ipv4_punched` tier is `not_offered` and P sends no `Register`. Otherwise P registers from its IPv4 socket exactly as A does.
9. **C replies to each `Register`.** For a live punch and a valid role ticket, C latches the first source it observes for that role, and it drops a later `Register` for the same role from any other source. It replies once per `Register`, to the sender only:
   - `Peer{punch_id, srflx}`, carrying the other role's latched mapping, once both roles are observed;
   - `Registered{punch_id}` before that.
10. **Both sides check `Peer` and punch.** Each side accepts `Peer` only when it comes from the resolved rendezvous address, carries its own `punch_id`, and names a `Public` IPv4 address. Each side **latches the first valid `Peer` per punch**: an identical later one is ignored, and one naming a different address is dropped and logged (`peer_conflict`). A adds the srflx IP to the allowed set for the rest of `PUNCH_TTL`. Both sides then punch the srflx (`PUNCH_BURST` at `PUNCH_INTERVAL`), and P starts a QUIC connect to A's srflx. P's own punches and retransmitted Initials open P's NAT, and A's punches open A's NAT.
11. **P picks a winner.** When the first QUIC handshake completes, P waits up to `TIER_GRACE` for a better-ranked attempt still in flight. It keeps the best one and closes the rest. If no QUIC handshake has completed within `QUIC_PHASE_BUDGET`, or every QUIC attempt has already errored, and the answer carries `tcp` candidates, P dials them concurrently (LAN before IPv6 before IPv4), each connect bounded by `DIRECT_LEG_DIAL_TIMEOUT`. QUIC attempts still in flight keep running, and the ranking still applies. The probe ends at `PROBE_BUDGET`.
12. **P proves the carrier.** P opens one session on the winning carrier (`DirectOpen{token, class: Api}`, then Noise IK). A successful handshake proves the carrier, and the resulting API leg is parked in the leg pool. Only then does the carrier become live on P.

```
P                                    C                                    A
│ POST /direct/{node}  {offer}  ───► │ admit · rate-limit · mint punch    │
│                                    │ ── DirectOffer{punch, offer,  ───► │ open(k_p2a) · freshness · replay
│                                    │     register?}                     │ allowed-IP set · gather
│                                    │ ◄── DirectAnswer{punch, answer} ── │ seal(k_a2p)
│ ◄── 200 {punch, answer, rdv?} ──── │                                    │ ── Punch×PUNCH_BURST ─► P host cands
│ open(k_a2p) · offer_id echo        │ ◄── Register{punch, gateway, t} ── │ (IPv4 socket, if register;
│                                    │ ── Registered ───────────────────► │  repeats until Peer)
│ ── auth Punch, QUIC Initial ══════════════════════════════════════════► │ LAN / ULA / GUA / public v4
│ ── Register{punch, device, t} ───► │                                    │
│ ◄── Peer{A srflx} ──────────────── │ ◄── Register (retry) ───────────── │
│                                    │ ── Peer{P srflx} ────────────────► │ latch · allow srflx IP
│ ── Punch×PUNCH_BURST ─► A srflx    │ (P's punches carry auth tags)      │ ── Punch×PUNCH_BURST ─► P srflx
│ ═══ QUIC Initial ─► A srflx ══════════════════════════════════════════► │
│ ═══ stream: DirectOpen{token, api} ▸ Noise IK ════════════════════════► │ same responder as relay
│ carrier live (best rank after TIER_GRACE); proof leg parked             │
```

**Roles.** P is always the QUIC client and A always the server.

- **ALPN.** Both sides set `DIRECT_QUIC_ALPN`.
- **Certificate.** A's certificate is a per-process `rcgen` self-signed certificate for `DIRECT_QUIC_SERVER_NAME`. P's `ServerCertVerifier`:
  - compares the SHA-256 of the end-entity certificate with `quic_cert_sha256` in constant time;
  - rejects any intermediates;
  - keeps rustls's `verify_tls13_signature` against the presented certificate;
  - allows TLS 1.3 only.

  QUIC's TLS is therefore authenticated and hides the `DirectOpen` token even from an active LAN attacker, but it still grants nothing: **Noise IK is the authentication for every session.**
- **Framing.** A `DirectOpen` is a u32-BE-length-prefixed JSON record capped at `MAX_DIRECT_FRAME_BYTES`, and it is the first thing on every stream or TCP connection. The Noise frames that follow use the same framing.
- **TCP.** On TCP the preface travels in plaintext, and the token stays valid for the runtime's lifetime. An on-path attacker who reads it gets past the gate and then fails at Noise, bounded by the pre-authentication caps.

### Wire types (PR1, `remote-host/crates/protocol/src/relay.rs`)

```rust
pub const DIRECT_OFFER: &str = "/direct/{relay_node_id}";
pub const DIRECT_PROTOCOL_VERSION: u16 = 1;
pub const MAX_RELAY_NODE_ID_BYTES: usize = 128;

/// 32 lowercase hex = 16 CSPRNG bytes, minted by C per punch.
pub struct PunchId(String);
/// 32 lowercase hex = 16 CSPRNG bytes, minted by C per punch and role.
pub struct RendezvousTicket(String);
/// 64 lowercase hex = 32 CSPRNG bytes, minted by A per binding runtime.
/// Zeroized on drop, redacted by `Debug`, compared in constant time.
pub struct DirectToken(Zeroizing<String>);

#[derive(Serialize, Deserialize)]
pub struct DirectCapability {
    pub version: u16,
    #[serde(default)]
    pub udp: bool,
}

#[derive(Serialize, Deserialize)]
pub struct ControlHello {
    pub relay_node_id: String,
    #[serde(default, deserialize_with = "lenient_capability", skip_serializing_if = "Option::is_none")]
    pub direct: Option<DirectCapability>,
}

/// Base64 of a 24-byte XChaCha20 nonce and of ciphertext‖tag, mirroring `NotifyRequest`'s `n`/`enc`.
#[derive(Serialize, Deserialize)]
pub struct SealedCandidates {
    pub n: String,
    pub enc: String,
}

#[derive(Serialize, Deserialize)]
pub struct UdpRendezvous {
    pub address: String,              // normalized `host:port` (UDP_PUBLIC_ADDR)
    pub ticket: RendezvousTicket,     // the recipient's role only
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlSignal {
    OpenDataLeg { relay_key: String, #[serde(default)] class: LegClass },
    /// Absent `register` ⇒ C has no UDP rendezvous: host candidates only.
    DirectOffer {
        punch_id: PunchId,
        offer: SealedCandidates,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        register: Option<UdpRendezvous>,
    },
}

/// A → C over `/control`, only ever in reply to a `DirectOffer`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlReport {
    DirectAnswer { punch_id: PunchId, answer: SealedCandidates },
    DirectDeclined { punch_id: PunchId },
}

#[derive(Serialize, Deserialize)]
pub struct DirectOfferRequest { pub offer: SealedCandidates }

#[derive(Serialize, Deserialize)]
pub struct DirectOfferResponse {
    pub punch_id: PunchId,
    pub answer: SealedCandidates,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous: Option<UdpRendezvous>,
}

/// First framed record on every direct stream / TCP connection.
#[derive(Serialize, Deserialize)]
pub struct DirectOpen { pub token: DirectToken, pub class: LegClass }
```

### Rendezvous and punch datagrams

Probe datagrams are binary, and their first byte never looks like QUIC:

```
byte 0   PROBE_DATAGRAM_MAGIC = 0x0b          (bits 0x80 and 0x40 clear)
byte 1   kind
  1 Register     punch_id[16] role[1] ticket[16]   zero-padded to REGISTER_DATAGRAM_LEN
  2 Registered   punch_id[16]
  3 Peer         punch_id[16] ipv4[4] port[2]
  4 Punch        seq[2] tag[16]                     (P → A: authenticated; A → P: random tag)
```

The codec is `ProbeDatagram::{encode, decode}`, in the protocol crate. C replies at most once per `Register`, only to its sender, and `REGISTER_DATAGRAM_LEN` exceeds every reply, so **C never emits more datagrams or bytes than it receives.**

**Punch authentication.** Every punch P sends carries `seq`, a per-probe counter, and `tag = HMAC-SHA256(k_punch, offer_id ‖ u16be(seq))[..16]`.

- A checks a tag against each of the binding's live punches in constant time and accepts each `(punch, seq)` once.
- On a match, A adds the datagram's source IP to that punch's allowed-IP set, up to `MAX_PRFLX_SOURCES_PER_PUNCH` sources. This admits P's real source even when no rendezvous observed it, for example a phone behind NAT reaching a public-IPv4 gateway.
- A never sends to an address it learned this way.
- A's own punches carry a random tag, and P drops every `Punch`.

## Constants

All values are planned (PR1 for protocol, C, A and carrier; PR2 for P).

| Value | Where | Buys | Costs |
|---|---|---|---|
| `MAX_UDP_HOST_CANDIDATES = 8`, `MAX_TCP_CANDIDATES = 4` | protocol | Decode caps for either side's sets | P hosts with many GUAs lose the lowest-ranked ones |
| `MAX_GATEWAY_GUAS = 2`, `MAX_GATEWAY_ULAS = 1`, `MAX_GATEWAY_IPV4_HOSTS = 2` | A | One stable GUA per /64; room for a private and a public IPv4 address | Hosts with more addresses lose the lowest-ranked ones |
| `VIRTUAL_INTERFACE_PREFIXES` | A | Container and VPN interfaces are never offered | A carrier over a VPN overlay is not attempted |
| `SEALED_OFFER_PLAINTEXT_LEN = 512`, `SEALED_ANSWER_PLAINTEXT_LEN = 1024` | protocol | Ciphertext length is constant per kind; each holds the full-cap all-IPv6 body, pinned by a test | 1.5 KiB of sealed data per probe |
| `MAX_SEALED_CANDIDATES_BYTES = 2048`, `MAX_DIRECT_OFFER_BODY_BYTES = 4 KiB` | protocol / C | Bounded parsing | — |
| `MAX_RELAY_NODE_ID_BYTES = 128` | protocol | Bounds every map keyed by node id | — |
| `OFFER_MAX_AGE = 120s`, `MAX_REPLAY_ENTRIES = 64` | A | Tolerates ordinary phone/gateway clock skew; the replay window is `2 × OFFER_MAX_AGE` | Skew over 2 min disables direct (logged as `declined:stale`) |
| `DIRECT_ANSWER_TIMEOUT = 3s` | C | A answers in milliseconds; bounds a parked POST | A wedged gateway costs a probe 3 s of background time |
| `PUNCH_TTL = 20s`, `MAX_DATAGRAMS_PER_PUNCH = 64` | C | Per-punch state only; covers both sides' full registration loops | A probe slower than 20 s loses its rendezvous |
| `REGISTER_RETRY_INTERVAL = 250ms`, `PEER_WAIT = 5s`, `REGISTER_DATAGRAM_LEN = 64` | A, P | Registration survives the loss of any datagram, including a `Peer` | ≤ 20 datagrams of 64 B per side per punch |
| `PUNCH_BURST = 5`, `PUNCH_INTERVAL = 200ms` | A, P | Covers the skew between the two `Peer` deliveries; later bursts pass the NAT opened by the earlier ones | See the caps below |
| `MAX_PUNCH_DATAGRAMS_PER_OFFER = 256`, `MAX_PRFLX_SOURCES_PER_PUNCH = 4` | A | Bounds A's fan-out and what authenticated punches can admit | Lowest-ranked pairs are not punched |
| `MAX_PUNCH_DATAGRAMS_PER_PROBE = 64` | P | Bounds P's fan-out; one QUIC attempt per remote candidate | — |
| `PROBE_BUDGET = 10s`, `QUIC_PHASE_BUDGET = 5s`, `TIER_GRACE = 300ms` | P | The whole probe runs in the background; TCP gets the second half; a later LAN success beats an earlier srflx one | None user-visible |
| `DIRECT_QUIC_KEEP_ALIVE = 10s`, `DIRECT_QUIC_IDLE_TIMEOUT = 45s` | carrier | Keepalive under common UDP NAT timeouts; the idle timeout equals the pump's `INBOUND_LIVENESS_TIMEOUT` (`app/ios/ffi/src/transport/pump.rs:28`), pinned by an ffi test | One small packet every 10 s |
| `DIRECT_QUIC_ALPN = "baybo-direct/1"`, `DIRECT_QUIC_SERVER_NAME = "baybo-direct"` | carrier | A fixed identity for the self-signed certificate | — |
| `MAX_DIRECT_FRAME_BYTES = 65535` | carrier | One frame carries one Noise message, whose maximum this is | — |
| `QUIC_STREAM_RECEIVE_WINDOW = 256 KiB`, `QUIC_CONNECTION_RECEIVE_WINDOW = 1 MiB` | carrier | Bounds what an admitted, unauthenticated peer can make A buffer | Upload throughput per stream ≤ window / RTT (≈ 5 MB/s at 50 ms) |
| `DIRECT_OPEN_DEADLINE = 1s`, `FIRST_STREAM_DEADLINE = 3s` | A | A silent stream or connection cannot hold a slot; the first-stream deadline starts at `Incoming::accept()` and covers the handshake | — |
| `MAX_QUIC_CONNECTIONS = 8`, `MAX_QUIC_CONNECTIONS_PER_SOURCE = 4`, `MAX_STREAMS_PER_CONNECTION = 32` | A | One carrier plus the losing probe attempts (≤ 3 per P source); streams cover the app's fan-out | — |
| `MAX_TCP_PREAUTH = 48`, `MAX_TCP_PREAUTH_PER_SOURCE = 4`, `TCP_PREAUTH_RESERVED = 24`, `MAX_TCP_SESSIONS_PER_DEVICE = 32` | A | Known sources keep 24 pre-auth slots, above the app's burst of 1 chat + 12 concurrent API dials + 3 pooled legs + blob | Unknown sources share 24 slots, 4 per IPv4 address or IPv6 /64 |
| `TCP_REBIND_DELAY = 2s`, `CARRIER_DRAIN_GRACE = 1s` | A | A failed listener retries; a stopping runtime drains briefly | — |
| `SOCKET_RECV_BACKOFF = 250ms · 2ⁿ, max 4s` | A, C | A receive error never tears down the runtime or C's HTTP/WSS | — |
| `UDP_SOURCE_WARN_INTERVAL = 60s` | C | Source-rewriting deployments stay visible without log floods | — |
| `DEFAULT_UDP_BIND_ADDR = 0.0.0.0:7777` | C | IPv4 rendezvous by default | — |
| `DIRECT_LEG_DIAL_TIMEOUT = 3s`, `CARRIER_DIAL_COOLOFF = 30s` | P | Bounds a blackholed carrier to one slow API dial; a stream-level failure pauses new carrier legs without dropping the carrier | ≤ 3 s once before the relay fallback |
| `CHAT_ROTATION_QUIET = 5s` | P | Rotation waits for a lull, not just a turn boundary | Chat moves over ≥ 5 s after a turn ends |
| `PROBE_BACKOFF = 1, 2, 4, 8, 15 min` (then 15), `CARRIER_MIN_LIFETIME = 60s` | P | Bounds load on C and A on networks where direct fails; a carrier that flaps counts as a failure | Up to 15 min on relay after a transient failure on a network that works |
| `NETWORK_SETTLE = 1s`, `NETWORK_KEY_LEN = 8`, `LOCAL_NETWORK_RETRY = 10s` | P | Probes start after the path settles (the debounce restarts on each change); the user may have just tapped Allow | Upgrade starts ≥ 1 s after a network change |
| `DIRECT_OFFERS_PER_SOURCE_PER_MINUTE = 6`, `DIRECT_OFFERS_PER_NODE_PER_MINUTE = 30`, `MAX_INFLIGHT_PUNCHES_PER_NODE = 2`, `MAX_PENDING_PUNCHES = 4096` | C (and A for in-flight) | Bounds forwarding work per source, per gateway and globally | A user flapping networks more than 6 times a minute waits for `Retry-After` |

## Connection policy on P

**Relay first.**

- `RelayDialer::establish` (`app/ios/ffi/src/relay/chat.rs:47`) never consults a carrier. A chat dial always goes to the relay.
- (PR2) `dial_tunnel_leg` (`app/ios/ffi/src/relay/tunnel.rs:304`, relay-only today) dials on the live direct carrier when it is usable (not suspended, not cooling off), and on the relay otherwise. If a carrier dial fails before Noise completes, or exceeds `DIRECT_LEG_DIAL_TIMEOUT`, P **immediately re-dials that leg on the relay**, then judges the carrier:
  - **QUIC.** P retires the carrier only on connection-level evidence: `Connection::close_reason()` is `Some`, or the connection received no datagram while the dial ran (`Connection::stats().udp_rx`). Any other failure belongs to the stream (A's caps, a slow device lookup): new legs go to the relay for `CARRIER_DIAL_COOLOFF`, and the carrier and a chat leg on it stay up.
  - **TCP.** Each leg is its own connection, so a connect failure or timeout retires the carrier, and a failure after connect starts the cool-off.

**The prober** lives in `app/ios/ffi/src/relay/carrier/` (PR2). It runs at most one probe per binding at a time, and **nothing waits on it**.

- Four events request a probe:
  - the relay chat leg becoming `Live`;
  - a satisfied, non-duplicate network change, debounced by `NETWORK_SETTLE` (the timer restarts on each change);
  - the app returning to the foreground;
  - the backoff timer firing while on the relay.
- A request runs only when all of the following hold: the binding is `ActiveLeg::Relay`, the app is active, no carrier is live, no probe is in flight, no `Retry-After` is pending, and the failure cache has no unexpired entry for the current network.
- The prober snapshots the **carrier epoch** before it binds sockets. A primary-network change, the background barrier, pairing and forgetting bump the epoch. A probe result from an older epoch is discarded, and its connection is closed.
- **Outcomes.**
  - Success clears the network's cache entry.
  - A failure advances the network's backoff: a `404` or `504`, an AEAD or `offer_id` failure, or a probe that ends on the relay.
  - `429` and `503` mean retry later: P waits for `Retry-After` and leaves the cache untouched.
  - A probe datagram send to a `Lan`-class or on-link destination that fails with `EHOSTUNREACH` or `EPERM` means the iOS Local Network permission is refused or still pending. That tier is `denied` and ends at once. A probe with a `denied` tier leaves the cache untouched and schedules one re-probe after `LOCAL_NETWORK_RETRY`, because the user may just have tapped Allow. A second consecutive such probe on the same network counts as a failure.

**Per-network failure cache.** The cache is keyed by `(BindingKey, NetworkKey)` and lives in memory only.

- `NetworkKey` is the first `NETWORK_KEY_LEN` bytes of `SHA-256("baybo/direct/network/v1" ‖ primary interface kind ‖ Wi-Fi/wired only: the primary interface's sorted IPv4 /24s and the path's gateway addresses)`. A cellular path is keyed by its kind alone: its IPv6 prefix and CGNAT address change on every re-attach and would reset the backoff.
- A failure advances that network's backoff step through `PROBE_BACKOFF`. A success clears the entry.
- A carrier that dies **unsolicited** within `CARRIER_MIN_LIFETIME` counts as a failure: a `CONNECTION_CLOSE` from A, a QUIC idle timeout, or a connection-level leg or rotation dial failure. A retirement P initiates (a failed foreground re-proof, `network_changed`, pair/forget, a stale epoch) never touches the cache.
- A new network key's first probe is immediate.

**Upgrade per class.**

- **Every carrier transition, up or down, calls `leg_pool::pool().invalidate()`** (`ApiLegPool::invalidate`, `app/ios/ffi/src/relay/leg_pool.rs:260`). Parked relay legs therefore stop serving once a carrier is live, and parked carrier legs die with their carrier. The proof leg is parked after the invalidation.
- **Api and Blob** legs dial on the carrier. An in-flight blob transfer is never migrated.
- On QUIC, A sends blob streams at a lower `SendStream::set_priority` than chat and API streams. Blob priority is therefore owned by the carrier, not by C's bandwidth classes.

**Chat rotation (idle only).** Answer deltas and reasoning are Ephemeral-plane frames and are never replayed ([`CONTEXT.md`](../../CONTEXT.md)). Switching legs mid-turn would therefore leave a visible hole in the answer. The supervisor (`app/ios/ffi/src/transport/supervisor.rs`) starts a rotation only when all of the following are true:

1. the leg is `Live` on the relay, with no dial in flight;
2. no session is in `Subscribing`;
3. the pump's **active-turn set** is empty;
4. no `Send` or `ResolveApproval` has been sent, and no non-keepalive frame received, for `CHAT_ROTATION_QUIET`.

The pump is the only writer of the active-turn set (PR2). It adds a session on `Frame::TurnState{active: true}`, removes it on `active: false`, and re-seeds the set from every `Frame::SubscribeState` turn snapshot (`app/ios/ffi/src/transport/pump.rs:332`). A latch stuck at `true` fails safe: the chat stays on the relay until the next re-subscribe.

Rotation is a new supervisor transition, `Live{rotating: Some(dial)}`:

1. Dial a chat session on the carrier (`DirectOpen{class: Chat}`, then Noise IK). Writing the `DirectOpen` is the rotation's **commit point**.
2. On success, install the new pump, re-enqueue `Subscribe` for every `Proven` session onto the new leg, and retire the relay pump **without fan-out**, the way `disconnect` does (`app/ios/ffi/src/transport/supervisor.rs:794`).

A's `LegDedup::install` (`crates/gateway/src/channel/state.rs:178`) aborts the displaced relay leg as soon as A's side of the Noise handshake completes, which is before P's side finishes. While a rotation is in flight:

- `Send` and `ResolveApproval` are held, for at most `DIRECT_LEG_DIAL_TIMEOUT`, and then flushed onto whichever leg is current when the rotation resolves. If no leg is live, they are refused as on a dead leg.
- A `PumpEnded` for the relay leg is held until the rotation resolves. On success the relay leg is retired silently; on failure the usual `leg_death` runs.

Before the commit point, the rotation is **abandoned** when the carrier dies, the network changes, the app backgrounds, the binding changes, or a turn starts. After it, the rotation ends only in success or dial failure. A turn that starts meanwhile is re-seeded by the re-subscribe (failure mode 11). A network or binding change retires the carrier, which fails the dial. A failed rotation dial follows the leg-dial rule above.

**Downgrade.**

- **Carrier death.** When the QUIC connection closes (idle timeout, error, or the gateway stopping), the legs on it end. The chat leg goes through the normal `leg_death` path, so ChatStore reconnects over the relay. That relay leg becoming live then requests a new probe.
- **Network change.** (PR2) `NWPathMonitor` (`app/ios/App/Core/PathMonitor.swift`) calls the synchronous ffi `network_changed(NetworkPath)`. The ffi keeps a fingerprint of the primary interface: its type, name and gateways.
  - An equal path (a duplicate delivery) is ignored.
  - A change confined to secondary interfaces (cellular appearing under Wi-Fi, an `isExpensive` or `isConstrained` flip) keeps the carrier.
  - A primary-interface change, or the carrier's local address leaving the path, bumps the carrier epoch, retires the carrier and aborts any probe before the call returns.

  Every satisfied, non-duplicate path requests a probe.
- **Background.** The `.background` barrier (`app/ios/App/BayboApp.swift:41-43`) already invalidates the leg pool. (PR2) It also bumps the carrier epoch, which aborts any probe and any uncommitted rotation, and **suspends** the carrier so that no new leg dials it. The QUIC connection and its open streams, including a rotated chat leg, are left alone, as a relay chat leg is. On `.active`, P re-proves a suspended carrier: `close_reason()` must be `None`, and a fresh proof leg must complete within `DIRECT_LEG_DIAL_TIMEOUT`; that leg is parked in the pool. Otherwise P retires the carrier, and a dead chat leg goes through `leg_death`, as a relay corpse found on foreground does ([`connection.md`](../../../app/ios/docs/connection.md)).
- **Evicting a carrier.** P closes that one connection, matched by `stable_id`. It never closes the shared endpoint, which other attempts on the same socket may be using.
- **Pairing changes.** `finish_pair` and `forget_pairing` (`app/ios/ffi/src/relay/pairing.rs`) clear the carrier state, epoch and failure cache infallibly.

**No route hints.** P persists no carrier state, and `forget_pairing` clears only in-memory carrier state. Every probe must deliver P's candidates to A, because A's allowed-IP set and firewall punches depend on them, and the probe runs off the critical path, so a remembered route would save nothing.

## Gateway (A)

**Lifecycle.**

- **Binding scope (PR1).** `relay_content::run` (`crates/gateway/src/channel/relay_content.rs:228`) is one loop today: each iteration resolves `approved_relay_settings` and runs one control connection (`run_once`). It splits into a **binding scope**, entered once per distinct `Ready(settings)` and left on Reconfigure, TearDown or shutdown, and an inner control redial loop. The scope owns the carrier runtime. A control redial re-resolves settings inside the scope and does not restart the runtime, so a control flap keeps carrier sessions up.
- **Settings identity (PR1).** `RelaySettings` (`relay_content.rs:127`, today `relay_url` and `remote_api_key`) gains `device_id`, `device_pubkey`, `auth_token_sha256` and `approved_at` from the approved `DeviceRow`. Its equality then covers the device identity and credentials, so a re-pair or credential change is a Reconfigure, just like a relay URL change.
- **Task tree.** Every QUIC stream task, before and after authentication, runs in its connection's `JoinSet`. Every connection task and TCP session task runs in the runtime's `JoinSet`.
- **Stopping.** A Reconfigure, a TearDown (revoke) or a shutdown stops the runtime:
  1. It shuts these `JoinSet`s down first, which drops in-flight router futures, the way `legs.shutdown()` hard-aborts relay legs (`relay_content.rs:506`). A request from a revoked device does not run to completion.
  2. It then closes the endpoints: `endpoint.close(CARRIER_REVOKED)`, then `wait_idle` bounded by `CARRIER_DRAIN_GRACE`.
- Revocation is noticed on the existing 5 s `DEVICE_POLL_INTERVAL` (`relay_content.rs:60`). That is the same bound as the relay control connection.
- The runtime is bound **before** control connects, so the capability in the hello reflects sockets that actually exist. It records `started_at_ms` for the offer freshness rule.

**Offers.**

- (PR1) `pump_control` (`crates/gateway/src/relay/mod.rs:192`) gains an outbound `mpsc<ControlReport>`.
- A `DirectOffer` is handed to the runtime through one narrow port: `CarrierRuntime::handle_offer(punch_id, offer, register) -> ControlReport`. The report is written back on the control connection that delivered the offer. If control redials in between, the report is dropped, and C answers P with `504`.
- A never sends a report unprompted.
- A holds at most `MAX_INFLIGHT_PUNCHES_PER_NODE` live punches, as defence in depth against a C that ignores its own limits. An offer that passes the checks supersedes the oldest (sequence step 4).

**QUIC endpoint.** A runs one long-lived `quinn::Endpoint` per family socket, serving many connections.

- **Admission.** A lets an `Incoming` proceed only when **its source IP belongs to the allowed-IP set of a live punch** and the connection caps, keyed by `source_key`, have room. The allowed set holds P's sealed host candidate IPs, the latched `Peer` srflx IP and the sources of authenticated punches, and it lives for `PUNCH_TTL`. Everything else is `ignore()`d: no response, no Retry, and no TLS work.
  - An admitted `Incoming` whose address is not yet validated gets `retry()`, which costs one RTT in a background probe.
  - A validated one is `accept()`ed.

  Matching by IP rather than address and port also admits a symmetric-NAT phone that reaches an open or full-cone gateway. The set is a gate against scanning and CPU use, not an authentication boundary.
- **Transport limits.** `crates/carrier` pins A's `TransportConfig`:
  - `max_concurrent_bidi_streams(MAX_STREAMS_PER_CONNECTION)`;
  - `max_concurrent_uni_streams(0)`;
  - `datagram_receive_buffer_size(None)`;
  - `stream_receive_window(QUIC_STREAM_RECEIVE_WINDOW)` and `receive_window(QUIC_CONNECTION_RECEIVE_WINDOW)`;
  - the keep-alive and the idle timeout.

  A never reads uni streams or datagrams, so it accepts none, and nothing can be buffered for them.
- **Streams.** `accept_bi` **spawns** each stream's `DirectOpen` + Noise IK authentication into the connection's `JoinSet`, bounded by a per-connection semaphore of `MAX_STREAMS_PER_CONNECTION`. Authentication never runs inline in the accept loop, so one stalled preface cannot block the next stream.
- **First-stream deadline.** `FIRST_STREAM_DEADLINE` starts at `Incoming::accept()`, so it covers the handshake. A connection with no authenticated stream by then is closed and releases its permit. So is a connection whose first stream fails authentication.
- **Hand-off (PR1).** TCP connections and QUIC streams share one routing function, `handle_authenticated_transport` in `crates/gateway/src/channel/carrier/`. Chat goes to `run_content_session` with a `LegDedup`; Api and Blob go to `run_tunnel_session`. Both become `pub(crate)` over the existing `BinarySink`/`BinarySource` seam (`crates/gateway/src/channel/device_content.rs:277-287`).
- **Receive errors.** A socket receive error backs off (`SOCKET_RECV_BACKOFF`) without tearing down the runtime. Only a persistent error rebinds the socket.

**Punching.** A sends punches only to two kinds of address:

- (a) host candidates from an offer that passed the seal, freshness and replay checks;
- (b) the latched `Peer` address of a punch that A itself answered, and only when it is `Public` IPv4 and comes from the resolved rendezvous address.

A never punches a source learned from an authenticated punch. C cannot choose any other target.

**TCP (opt-in).** TCP runs only when `gateway.direct_tcp` is present.

- The listeners bind at runtime start, and a failed bind or accept rebinds after `TCP_REBIND_DELAY`.
- **Before the preface,** the accept loop takes a pre-authentication permit with `try_acquire`, never waiting for one, and closes a connection it cannot admit:
  - global `MAX_TCP_PREAUTH`, and `MAX_TCP_PREAUTH_PER_SOURCE` per `source_key`, so an IPv6 client rotating through its /64 is one source;
  - a source in a live punch's allowed set, or one that has authenticated in this runtime, is exempt from the per-source cap and may use `TCP_PREAUTH_RESERVED` permits that no other source can take.

  The permit is released when Noise completes or fails, so a flood of unknown sources cannot starve P's legs.
- An authenticated session holds a per-device permit (`MAX_TCP_SESSIONS_PER_DEVICE`).
- `DirectOpen` must arrive within `DIRECT_OPEN_DEADLINE`, and the token is compared in constant time.
- Each leg is its own TCP connection.

**Dedup.** `LegDedup` (`crates/gateway/src/channel/state.rs:174-183`) spans carriers unchanged: whichever chat leg installs last wins, and A does not rank carriers against each other. P's supervisor is the only party that decides which chat leg is current, and it never has two chat dials out at once. A late relay handshake cannot occur, because P dials a relay chat leg only when it has no live chat leg.

**Link table.** `DeviceLinks` (PR1) is an in-memory table per device. It records the live legs, each with its `LegClass`, its `CarrierKind` and a start time, together with the last offer's time and outcome. The relay path and the carrier runtime are its only writers. A QUIC carrier's kind comes from the address pair:

- `Lan` when both ends are `Lan`;
- `Ipv6` for a GUA pair;
- `Ipv4` when A's local address is public IPv4;
- `Ipv4Punched` when A's local address is private and P's is public IPv4, meaning P reached A through A's NAT mapping.

P derives the same kind from the A candidate it dialed.

### Config

`gateway.*` is not hot-reloadable; changes take effect on restart. **`crates/config/src/validate.rs` is the only home of the rules.**

- The runtime receives typed values through `RuntimeGatewayConfig` (`crates/gateway/src/config.rs`) and copies them without re-checking.
- `validate.rs` owns shape rules. `AddressPolicy` owns address classes, and the gateway applies it when it gathers candidates. Neither re-implements the other, so `baybo-config` does not depend on the protocol crate.

```jsonc
"gateway": {
  // On by default whenever a binding exists; absent ⇒ defaults.
  "direct_udp": { "enabled": true, "ipv4_bind": "0.0.0.0:0", "ipv6_bind": "[::]:0" },
  // Opt-in: absent ⇒ no TCP listeners (the Option-section convention of config.md).
  // No default ports: the operator picks them and forwards or advertises them.
  "direct_tcp": { "ipv4_bind": "0.0.0.0:<port>", "ipv6_bind": "[::]:<port>",
                  "advertised_addresses": ["<public-ip>:<forwarded-port>"] }
}
```

The fields are typed `Option<SocketAddr>` or `Vec<SocketAddr>`, so an unparseable address fails deserialisation. `validate_gateway` (`crates/config/src/validate.rs:259`) adds these rules:

- **Family:** each `*_bind` must match its family.
- **At least one family:** `direct_udp.enabled` requires at least one bind, and a present `direct_tcp` needs at least one bind.
- **Advertised addresses:** they must have a non-zero port and must not be unspecified.

`direct_udp.enabled: false` is the off switch.

A gateway in a container needs host networking for direct UDP. On a bridge network, the host's NAT can confirm conntrack state for P's early inbound punch before A's own first outbound, and it then remaps A's port.

## C (remote-host)

- **Routes.** C adds one HTTP route, `POST /direct/{relay_node_id}`, behind the same `require_admitted` route layer and outer per-IP limiter as the five relay routes (`remote-host/crates/relay/src/serve.rs:331-345`). It adds one UDP socket.
  - `is_relay_route` includes `/direct/`, so refusals on this route are logged.
  - The edge traffic ledger records the route as `direct/offer`.
  - POST is used because the request carries a body, mints state, and must never be cached.
- **No device authentication at C.** C does not authenticate the device on this route: A does, through the seal.
  - A holder of the tenant key and the node id can make C forward garbage. A declines it after one AEAD open, with no punch and no registration.
  - On the built-in proxy every device shares the `guest` key ([`relay-push-security.md`](relay-push-security.md#shared-relay-key-tenancy-and-push-binding-authentication)), so the owner check is not a tenant boundary there, and the node id is the only gate. The node id is not secret: it appears in `/content/join/{relay_node_id}` URLs, which a fronting CDN sees, and in the gateway's info-level log.
  - The offer budget is therefore keyed per (node, client IP) under a higher per-node ceiling, so one source cannot spend another's share. The client IP comes from the shared client-IP resolution the per-IP limiter uses (`CLIENT_IP_HEADERS`).
- **Punch registry** (`remote-host/crates/relay/src/punch.rs`, PR1). The registry maps `PunchId → { node, owner key, control token, answer oneshot, tickets per role, latched srflx per role, datagram count, created }`.
  - A punch counts toward `MAX_INFLIGHT_PUNCHES_PER_NODE` from creation until one of these: its POST ends with a non-200 status, or with a 200 that carries no rendezvous (both drop the entry at once); or C has sent `Peer` to both roles. After that the entry only answers re-sent `Register`s until `PUNCH_TTL`.
  - Every `429` carries `Retry-After`: for the in-flight cap, the time until the node's oldest in-flight punch expires; for a rate, the time until its window has room.
  - Entries expire at `PUNCH_TTL` through a sweep. All of a node's punches drop when that node's control connection unregisters.
  - The registry enforces `MAX_INFLIGHT_PUNCHES_PER_NODE` and `MAX_PENDING_PUNCHES`.
  - C keeps **no per-gateway UDP state outside a punch**: no heartbeat and no standing registration.
  - (PR1) `MAX_RELAY_NODE_ID_BYTES` is enforced wherever a node id becomes a map key. `ControlRegistry::register` gains the check (`serve.rs:760` registers a node id of any length today), and punch creation applies it.
- **Control reports.** (PR1) The inbound branch of `run_control` (`serve.rs:801-806`), which today treats every gateway frame as liveness, parses binary frames as `ControlReport`. A report is honoured only for a punch owned by *this* node's *current* control token. A malformed or foreign report is logged at debug level and ignored, and **never closes control**.
- **UDP socket** (`remote-host/crates/relay/src/udp.rs`, PR1).
  - **Knobs.** `.env` gains `UDP_PORT` (default 7777, the same on host and container). The compose file publishes it as `0.0.0.0:${UDP_PORT}:${UDP_PORT}/udp` and sets `UDP_BIND_ADDR: 0.0.0.0:${UDP_PORT}`, mirroring `PORT` → `BIND_ADDR`. Without the variable, the process binds `DEFAULT_UDP_BIND_ADDR`.
  - The socket starts only when `UDP_PUBLIC_ADDR` is set. A `UDP_BIND_ADDR` that cannot receive IPv4 fails startup.
  - `UDP_PUBLIC_ADDR` is normalised at startup: it is a `Public` IPv4 literal or an LDH hostname, plus a port. A DNS name is not resolved at C, and an invalid value fails startup. Clients resolve it with `resolve_public_v4`.
  - The rendezvous is IPv4 by design, because its job is to observe IPv4 NAT mappings. IPv6 connectivity needs no C. `::ffff:` sources are canonicalised. A source that is not IPv4 is dropped (`udp_source_not_ipv4`), and so is one that is not `Public` (`udp_source_not_public`). Both warnings are rate-limited to one per `UDP_SOURCE_WARN_INTERVAL`, which makes a source-rewriting deployment visible.
  - A `Register` is answered only for a live punch with a valid role ticket, compared in constant time, and only from the source latched for that role (sequence step 9). Anything else is dropped silently, so C offers no oracle.
  - Datagrams per punch are capped at `MAX_DATAGRAMS_PER_PUNCH`.
  - Receive errors back off (`SOCKET_RECV_BACKOFF`) without stopping HTTP or WSS.
- **What C logs.**
  - Per offer: node, `key_tag`, a `key_tag` of the punch id, status, elapsed time.
  - Per punch: whether each role registered, and the time to `Peer`.
  - **Never** tickets, sealed blobs, or the observed addresses at info level. Addresses appear at debug level only.

## Compatibility and rollout

| A | C | P | Outcome |
|---|---|---|---|
| any | any | old | P never posts. Nothing changes. |
| old | new | new | No capability in the hello ⇒ `404`. P stays on the relay and backs off. |
| new | old | new | The route does not exist ⇒ `404`, same as above. The old C ignores the extra hello field (serde ignores unknown fields). |
| new | old | old | A binds its sockets and advertises; nothing ever arrives. |
| new, `direct_udp.enabled: false`, no TCP | new | new | No capability ⇒ `404`. |
| new | new, no `UDP_PUBLIC_ADDR` | new | Offers flow without `register`: LAN, IPv6, public-IPv4 host candidates (admitted by authenticated punches) and TCP work; `Ipv4Punched` does not. |
| new | new | new | Full design. |

The built-in public proxy (`proxy.baybo.space`) runs with the UDP rendezvous enabled. A self-hosted C opts in by setting `UDP_PUBLIC_ADDR`.

**Deploy order:** C, then gateways, then the app. Any order is safe, but only C-first gives the app something to use on its first release. `DirectCapability.version` and `CANDIDATE_SET_VERSION` carry future changes: an unknown version reads as "not capable", never as an error.

## Security

This section records the delta against [`relay-push-security.md`](relay-push-security.md#security-boundaries).

**New in-scope parties:**

- an on-path LAN attacker between P and A;
- network observers on the direct path;
- C forging, replaying or withholding candidate advertisements and `Peer` messages.

**C can, in addition:**

- **Observe new metadata.** C sees P's and A's IPv4 UDP mappings (ip:port), their NAT behaviour as it is visible from one vantage point, and the time, frequency and C-side outcome of every direct attempt.
- **Force the relay.** C can drop, delay or answer falsely to `POST /direct`, `DirectOffer`, `DirectAnswer`, `Register` or `Peer`. The effect is availability only: P stays on the relay.
- **Steer registrations, punches and one admission.** For each offer that P really sealed, C chooses:
  - one `Public` IPv4 rendezvous address, via an A-record lookup of a name C picked. A and P each send it at most `PEER_WAIT / REGISTER_RETRY_INTERVAL` datagrams of `REGISTER_DATAGRAM_LEN` bytes;
  - one latched `Public` IPv4 srflx address per side. It receives `PUNCH_BURST` punches from the other side and, from P, QUIC Initials for up to `PROBE_BUDGET`;
  - one `Public` IP in A's allowed set for `PUNCH_TTL`: the srflx it names for P.

  That admission lets C reach A's pre-authentication QUIC surface and fetch A's per-process certificate. It still ends at the token gate and Noise.

**C cannot, assuming endpoint keys stay secret:**

- **See host candidates.** C cannot read P's or A's LAN addresses, ULAs, GUAs or public IPv4 interface addresses, or the direct token. It cannot learn how many candidates either side has, or whether A offers TCP.
- **Tamper with sets.** C cannot inject, alter or drop an individual host candidate. Any tampering fails the AEAD, and the whole set is rejected.
- **Point either side at a private address.** C cannot make A or P send to a `Lan` or excluded address. Both sides require the rendezvous and `Peer` addresses to be `Public` IPv4, and private targets come only from sealed, authenticated sets.
- **Admit any other source at A.** An authenticated punch needs `k_punch`, so C cannot forge one. An observer on the P→A path can race a copy from its own address, which admits that address to the same pre-authentication surface, at most `MAX_PRFLX_SOURCES_PER_PUNCH` per punch.
- **Replay an old answer to P.** The `offer_id` echo prevents it.
- **Get a replayed offer accepted.** The replay cache declines a repeat within a runtime, and a runtime declines any offer issued before it started.
- **Mint an offer that A accepts.** Doing so needs P's static secret.
- **Read, modify or misroute content on a direct carrier**, or impersonate either end. This is Claim 5.

**Claim 5: a carrier session is exactly as trustworthy as a relay leg.** A carrier session delivers no application byte before Noise IK completes between the pairing statics. A checks the initiator against its approved device rows (`lookup_approved_by_pubkey` in `responder_handshake`, `crates/gateway/src/channel/device_content.rs:212`), and P checks the pinned gateway static. The `DirectOpen` token, QUIC's TLS and the allowed-IP set are availability gates only. Claim 2 therefore holds verbatim with "carrier" in place of "relay leg". The sealed candidate sets and the `Public`-IPv4 checks add the guarantee that C cannot steer either side's direct traffic toward any private address. Toward public addresses, C can steer only what *C can* lists above, per genuine offer.

**DoS and amplification.**

- **C's UDP** replies at most once per `Register`, only to its sender, never with more bytes than it received, and only for live punches with the correct role ticket. It cannot reflect or amplify.
- **`POST /direct`** is bounded by the per-IP bucket, the per-(node, source) rate, the per-node ceiling and the in-flight cap.
- **A** does no handshake work for sources outside its allowed set, sends a Retry to unvalidated ones, and pins its QUIC transport limits so that an admitted, unauthenticated peer cannot make it buffer data it never reads. QUIC's 3× anti-amplification limit applies to the admitted sources.
- **A's TCP listener** takes pre-authentication permits without waiting, keyed by `source_key`, and keeps a reserved share for known sources.
- **P** sends at most `MAX_PUNCH_DATAGRAMS_PER_PROBE` punches and one QUIC attempt per remote candidate per probe, only toward sealed candidates it may dial and the one latched srflx.

**LAN probing.**

- A's private targets are exactly the addresses the legitimate P sealed about itself. P omits cellular private addresses.
- A may still punch a P LAN address that coincides with an unrelated host on A's own LAN, for example when P is on a different Wi-Fi that uses the same subnet. That costs `PUNCH_BURST` small, inert datagrams per address per genuine probe.
- P dials A's `Lan` candidates only from a Wi-Fi or wired interface that has a `Lan` address of that family: at most `PUNCH_BURST` punches and one QUIC attempt per candidate per probe. With `direct_tcp`, a TCP dial on a foreign Wi-Fi that reuses A's subnet can hand an unrelated host the `DirectOpen` token. The token passes only the gate, and every session still needs Noise IK.

**Privacy delta.**

- A and P now learn each other's addresses. On the relay, each saw only C.
- Network observers on either path see that P talks to A.
- C sees less traffic metadata while a carrier is live, because that traffic bypasses it.

Browser remote access is out of scope and is not affected.

## Observability

- **Carrier model.** `CarrierKind { Relay, Lan, Ipv6, Ipv4, Ipv4Punched, Tcp }` is shared by A's link table and P's state. A probe has one tier per direct kind (`lan`, `ipv6`, `ipv4`, `ipv4_punched`, `tcp`), and records an outcome for each:
  - `ok`, `failed` or `timeout`;
  - `not_offered`: no candidates of that tier, no rendezvous, or a path that may not dial it;
  - `denied`: the iOS Local Network permission is refused or pending (see *The prober*);
  - `skipped`.
- **iOS.** (PR2) `SettingsScreen` (`app/ios/App/Screens/SettingsScreen.swift`) gets a **Connection** row showing "Relay", "Direct · LAN", "Direct · IPv6", "Direct · IPv4", "Direct · IPv4 (punched)" or "Direct · TCP". A **Last probe** detail shows the time, the network kind and the per-tier outcomes. State reaches Swift through a new `CarrierSink` callback interface, registered like the existing sinks (`app/ios/ffi/src/lib.rs:149-171`). **The chat screen gets no badge**: the chat header keeps showing only `legDown`.
- **Gateway.** `baybo device status` (PR1) lists, for each approved device, its live legs by class with their carrier, and its last offer outcome.
  - It reads the running gateway's admin route `GET /v1/mobile/links`, authenticated with the vault's admin token. It is the first CLI command that queries the running gateway instead of the stores. When the gateway is not reachable, it says so and prints the device rows alone.
  - It is shell-only, like the rest of the `device` family, which `crates/cli/src/slash.rs` already rejects as a whole.
- **Structured logs,** as key=value fields. Addresses appear only at debug level.
  - **P:** `direct_probe probe trigger network=<tag> outcome elapsed_ms tiers="lan=… ipv6=… ipv4=… ipv4_punched=… tcp=…"`, `direct_attempt tier family candidate_class outcome elapsed_ms`, `peer_conflict punch`, `carrier_up kind`, `carrier_down kind reason lifetime_ms`, `carrier_cooloff reason`, `chat_rotation outcome waited_ms`.
  - **A:** `direct_offer punch=<tag> device outcome=accepted|declined:<auth|stale|replayed|over_cap>` (plus `superseded=<tag>` when it replaced a punch), `udp_register punch outcome`, `peer_conflict punch`, `punch punch targets datagrams`, `punch_admit punch sources`, `quic_incoming outcome=accepted|retried|ignored:<not_in_punch|cap>` (rate-limited to counters), `carrier_session class kind device`.
  - **C:** as listed under *C (remote-host)*.

## Failure modes

| # | Scenario | Detection | Behaviour | User cost |
|---|---|---|---|---|
| 1 | C, A or P lacks support | `404` | Stay on the relay; back off | none |
| 2 | A's side is symmetric NAT, P is symmetric and A port-restricted, or UDP is blocked | No QUIC handshake within `QUIC_PHASE_BUDGET` | Try TCP if offered, else relay; back off per network | none |
| 3 | Carrier dies (gateway restart or crash, NAT rebind) | `CONNECTION_CLOSE` on a graceful stop; otherwise the QUIC idle timeout or the pump's liveness | Legs end; chat reconnects over the relay; re-probe | One reconnect. After a crash, a chat leg on the carrier stays dead for up to `DIRECT_QUIC_IDLE_TIMEOUT`, where the relay notices within an RTT |
| 4 | Carrier blackholed (mapping expired silently) | A carrier dial times out with no datagram received | That leg falls back to the relay; carrier retired | ≤ 3 s on one API call |
| 5 | Stream-level failure on a live QUIC carrier | A carrier dial fails while datagrams still arrive | That leg falls back to the relay; carrier cools off; the chat leg is untouched | ≤ 3 s on one API call |
| 6 | Primary network change | `network_changed` | Carrier retired synchronously; relay; re-probe after `NETWORK_SETTLE` | One chat reconnect if chat was on the carrier |
| 7 | App backgrounded | `.background` barrier; re-proof on `.active` | Carrier suspended; kept if the re-proof succeeds, otherwise retired, chat reconnects over the relay, then re-probe | none if kept; one chat reconnect otherwise |
| 8 | Clock skew beyond `OFFER_MAX_AGE` | A: `declined:stale` with the skew logged | Relay | none; the operator sees the skew in A's log |
| 9 | Local Network permission refused or pending | A probe send to a `Lan` or on-link address fails with `EHOSTUNREACH`/`EPERM` | LAN and on-link IPv6 tiers `denied`; other tiers still run; one re-probe after `LOCAL_NETWORK_RETRY` | Same Wi-Fi: `Ipv4Punched` via hairpin, or relay |
| 10 | Control redials between offer and answer | C: no report | `504`; back off | none |
| 11 | A turn starts after the rotation's commit point | Re-subscribe returns `SubscribeState{active}` | State re-seeded; deltas between A's abort and the re-subscribe are lost, as on any reconnect | A rare, short gap, identical to a relay reconnect |
| 12 | Rotation dial fails | The dial fails | Leg-dial rule; if A already switched, chat reconnects over the relay | At most one reconnect |
| 13 | C at capacity | `429` / `503` | Retry after `Retry-After`; failure cache untouched | none |
| 14 | IPv6-only path without CLAT | No IPv4 route | IPv4 tiers `not_offered`; IPv6 and TCP still run | Relay if A has no GUA and no TCP |
| 15 | Device revoked | 5 s poll → TearDown | Runtime stops; carrier sessions and their in-flight requests dropped; relay Noise lookup fails too | By design |

## Testing

**Unit tests (PR1):**

- **protocol:**
  - serde round trips; an old hello decoding with `direct: None`; lenient decode of a malformed capability;
  - the `AddressPolicy` class table, `source_key`, and the production policy rejecting a loopback or `Lan` rendezvous address in `resolve_public_v4`;
  - the `ProbeDatagram` codec, including a property test that every encoded datagram has `0x80|0x40` clear and that a `Registered` or `Peer` is never longer than a `Register`.
- **device-proto:** seal/open round trip; directional separation (a set cannot be opened with the other direction's key); AAD binding (a wrong node id or punch id fails); an all-zero peer key is rejected; tampering fails; an empty set and a full-cap all-IPv6 set seal to the same length, as do an answer with an empty `tcp` and one with a full `tcp`; an oversized body fails to seal; a wrong-length plaintext fails to open; punch tags verify and reject.
- **carrier:**
  - demux routing, including a synthetic GRO buffer holding `Peer ‖ Registered` that yields the `Peer`; burst pacing (`start_paused`); `DirectOpen` framing bounds;
  - the `grease_quic_bit` transport parameter is absent in both directions;
  - a server presenting a different self-signed certificate fails the handshake before any stream opens;
  - A refuses a uni stream; a handshake that never completes releases its permit at `FIRST_STREAM_DEADLINE`.
- **relay (C):**
  - `POST /direct` returns `401`/`404`/`429`/`503`/`504`/`200` with owner and capability gating;
  - the per-(node, source) budget keys on the header-resolved client IP;
  - per-node caps; the in-flight release rules; the punch TTL sweep; a control disconnect drops punches;
  - a malformed capability or report keeps control;
  - role tickets are not interchangeable; unknown punch ids get no reply; a `Register` for an observed role from a new source is dropped; non-IPv4 and non-`Public` sources are ignored;
  - per punch, C emits no more datagrams or bytes than it receives;
  - receive errors back off.
- **gateway:**
  - offers are declined on auth, stale (including an offer issued before the runtime started), replay and a full replay cache; a new offer supersedes the oldest punch at the cap;
  - `Peer` is latched (a conflicting one is logged and dropped), and a lost first `Peer` is recovered by the next `Register`;
  - a valid authenticated punch admits its source and a bad tag does not; an `Incoming` outside the allowed set is ignored; an unvalidated one gets a Retry;
  - a stalled preface on stream 1 does not block stream 2; the first-stream deadline holds;
  - `stopping_a_binding_closes_its_active_direct_sessions` passes for TCP and QUIC, including a QUIC Api leg parked in a slow handler whose future is dropped on revoke;
  - interfaces are not enumerated when nothing is bound;
  - the TCP per-source cap keys an IPv6 /64 as one source, and a pre-auth flood leaves known sources their reserved permits;
  - `LegDedup` works across carriers; the `CarrierKind` classification; the config validation table.

**E2E (PR1)** lives in `crates/gateway/src/channel/relay_e2e.rs`, which already boots a real in-process C. It builds with `test-support`, so `AddressPolicy::for_tests()` treats 127/8 as `Public` and `::1` as `Lan`. It adds a mock device that seals an offer, posts it, registers, and connects over QUIC:

- the rendezvous and `Ipv4Punched` path runs over 127.0.0.1;
- the `Lan` carrier runs over `[::1]`;
- each must complete a chat frame round trip, and a TCP leg must work when TCP is configured;
- revoking mid-session must close the carrier sessions.

**Unit tests (PR2):**

- the prober state machine: single flight, backoff steps, a new network key probing immediately, stale-epoch discard, `429` leaving the cache alone, `denied` scheduling one re-probe, a P-initiated retirement leaving the cache alone, a cellular path not dialing `Lan` candidates;
- `network_changed`: two identical paths cause no retirement; cellular toggling under Wi-Fi causes none; Wi-Fi to cellular causes exactly one;
- rotation: the idle predicate, the commit point, sends held during a rotation, `PumpEnded` held during a rotation, each abandonment cause;
- a stream-level failure keeping the carrier; the background suspend and foreground re-proof;
- pool invalidation on every transition; `forget_pairing` clearing carrier state.

**The netns NAT matrix (PR2)** is `#[ignore]`d because it needs root. A non-gating `netns-matrix` CI job (`continue-on-error`, PR-only, path-filtered on the carrier code) runs `scripts/netns-matrix.sh`. The script builds the three workspaces' binaries with `--features test-support`, passes their paths to the test through environment variables, and runs `sudo -E cargo test … -- --ignored`. It drives three real processes, each in its own namespace:

- C: the `remote-host` binary;
- A: the `baybo` gateway binary, so the gateway under test is the shipped entry point. A `test-support` seed example first writes an approved relay binding (the device row and relay settings) into its workspace's stores, then exits;
- P: the ffi client, run from an example binary with the in-memory keychain backend (PR2, `test-support`) seeded with the matching `PairedRecord`.

The test lives in `app/ios/ffi/tests/netns_matrix.rs`, with the topology script in `scripts/netns-matrix.sh`.

```
 ns:a ── ns:nat-a ──┐                        ┌── ns:nat-p ── ns:p
 10.0.1.2 · 2001:2:0:a::2   ns:inet (router)  10.0.2.2 · 2001:2:0:b::2
                    ├──── 198.18.0.0/24 ─────┤
                    │     2001:2::/64        │
                    └──────── ns:c ──────────┘   198.18.0.10 (HTTPS/WSS + UDP rendezvous)
                                                 2001:2::10  (HTTPS/WSS only, IPv6 variant)
 same-LAN variant: ns:p on ns:a's bridge (10.0.1.3)
```

Every NAT namespace sets `net.netfilter.nf_conntrack_udp_timeout=30` and `nf_conntrack_udp_timeout_stream=30`, both per namespace, so that the keepalive is exercised. Every profile's input chain drops unsolicited WAN traffic before conntrack confirms it, as a consumer router does:

```
chain input { type filter hook input priority 0; policy accept; iifname "wan" ct state new drop }
```

Without that chain, an early inbound punch pins a conntrack entry, and masquerade then remaps the host's port. Each NAT box applies one profile to UDP (nftables):

- `open`: routed; no NAT, no filter.
- `cone` (endpoint-independent mapping and filtering): `snat to <wan>`, which preserves the port, plus `udp dport 1024-65535 dnat to <host>`.
- `port-restricted` (endpoint-independent mapping, address-and-port-dependent filtering): `masquerade` (conntrack filters replies to the exact remote).
- `symmetric` (address-and-port-dependent mapping): `masquerade fully-random`.
- `v6-firewall`: IPv6 is routed but not NATed. Forwarded traffic is accepted when `ct state established,related`, and new inbound connections from the WAN interface are dropped.
- `udp-blocked`: `meta l4proto udp drop` on forward.
- `nat64`: nat-p runs a `jool` NAT64 for an IPv6-only P without CLAT.

Expected IPv4 results, with no IPv6 and different networks. Each cell is the carrier P ends on:

| P ↓ \ A → | open | cone | port-restricted | symmetric |
|---|---|---|---|---|
| open | `Ipv4` | `Ipv4Punched` | `Ipv4Punched` | `Relay` |
| cone | `Ipv4` | `Ipv4Punched` | `Ipv4Punched` | `Relay` |
| port-restricted | `Ipv4` | `Ipv4Punched` | `Ipv4Punched` | `Relay` |
| symmetric | `Ipv4` | `Ipv4Punched` | `Relay` | `Relay` |

Additional rows and variants:

- same LAN → `Lan`;
- both sides `v6-firewall` with GUAs → `Ipv6`, whatever the IPv4 profile;
- `udp-blocked` on either side, with A `open` and `direct_tcp` set → `Tcp` (after `QUIC_PHASE_BUDGET`), otherwise `Relay`;
- C without `UDP_PUBLIC_ADDR` → A `open` gives `Ipv4`, and every other IPv4 cell gives `Relay`;
- P IPv6-only behind `nat64` → `Ipv6` when A has a GUA, otherwise `Relay`;
- P reaching C's HTTPS over IPv6 → the IPv4 table unchanged, because admission never depends on the HTTPS client IP;
- `tc netem loss 5%` on both WANs, port-restricted × port-restricted → `Ipv4Punched`.

Each cell runs five times to catch order-dependence. Every run asserts, in order (steps 2 to 4 only for a cell that ends on a direct carrier):

1. the first chat leg went over the relay;
2. the final `CarrierKind`, and one API request on it;
3. after an idle phase of at least 40 s, with rotation held off through a `test-support` knob so that only QUIC keepalives cross the NATs, the `CarrierKind` is unchanged and a second API request runs on the carrier with no relay fallback;
4. once rotation is released, the chat rotates and a chat frame round-trips on the carrier.

**Real-device checklist.** The owner runs this before PR2 is marked ready:

1. **Same Wi-Fi.** "Direct · LAN" appears within seconds of opening the app, and the Local Network prompt appears at most once. On first launch, "Direct · LAN" appears within `LOCAL_NETWORK_RETRY` of tapping Allow.
2. **Cellular with IPv6.** "Direct · IPv6".
3. **Cellular with IPv4 only.** "Direct · IPv4 (punched)" or "Relay", with the per-tier outcomes shown.
4. **Walk out of Wi-Fi mid-conversation.** There is at most one reconnect, the app falls back to the relay, and it then re-upgrades.
5. **Switch apps for under 5 s mid-reply.** The answer has no hole, and the carrier is kept.
6. **Background for over a minute, then foreground.** The carrier is re-proven, or the chat resumes over the relay and then rotates.
7. **A long reply started on the relay.** No rotation happens until the turn ends, and the answer has no hole.
8. **Blob upload and download over a direct carrier.**
9. **Gateway restart while on a direct carrier.** The app recovers to the relay, then re-upgrades.
10. **`baybo device revoke`.** The app's carrier legs die within 5 s.
11. **`baybo device status`** matches the app's Connection row.

## Deploying C (operators)

- **Set `UDP_PUBLIC_ADDR`** to an IPv4 literal plus port, or to a hostname plus port whose A record points at C. The hostname must be **DNS-only** and must not carry an AAAA record: a CDN-proxied name cannot carry UDP and would hide the NAT mapping, and clients resolve A records only. It is separate from the relay hostname, which may stay behind a CDN. If it is unset, the rendezvous is off, but offers are still forwarded.
- **Open the UDP port inbound** (`.env` `UDP_PORT`, default 7777/udp) in the host firewall and in any provider security group. C needs no IPv6 for this feature.
- **Preserve the source address** when publishing the port from a container. The compose file publishes `0.0.0.0:${UDP_PORT}:${UDP_PORT}/udp` through the kernel NAT path (the default iptables/nftables DNAT publishing). A userland proxy rewrites the source to a private bridge address. C then rejects every registration and logs `udp_source_not_public`.
- **On a multi-homed host running C with host networking,** set `UDP_BIND_ADDR` to the public-facing address. Replies must leave from the address the clients sent to, because clients drop a `Peer` from any other source.
- The existing `RELAY_IP_*` per-IP limits cover `POST /direct`. The per-source and per-node offer limits are fixed constants.

## Invariants

1. **The relay is never gated.** No dial waits on a probe, and the chat dial path never uses a carrier. Rotation is the only way a chat leg reaches a direct carrier, and it starts only when the chat is idle.
2. **Every carrier session runs `DirectOpen`, then Noise IK between the pairing statics, then the same responder as a relay leg.** A resolves the device only by its static key.
3. **Pairing is relay-only.**
4. **A's carrier runtime lives in the binding scope**, which exists only while the binding resolves to one `Ready(settings)`; a control redial does not restart it. Stopping it drops every carrier session's task, pre- and post-authentication, for TCP and QUIC, before it closes the endpoints.
5. **One stable UDP socket per family per side.** Registration, punches and QUIC share it. A's lives for the runtime and rebinds only on a persistent socket error; P's lives for a probe and its carrier. Receive errors back off and do not tear down the runtime.
6. **Every endpoint sets `grease_quic_bit(false)`**, so its peer never clears the fixed bit toward it, and every probe datagram's first byte has `0x80|0x40` clear. The demux splits GRO buffers into segments and is built on `quinn_udp`, so replies keep the targeted source address.
7. **A admits an `Incoming` only when its source IP is in a live punch's allowed set,** retries an unvalidated address, and pins its QUIC transport limits. Everything else is `ignore()`d.
8. **Stream authentication is spawned, never inline in `accept_bi`.** A connection gets `FIRST_STREAM_DEADLINE`, counted from `Incoming::accept()`, to authenticate a stream.
9. **Caps are named by what they count and keyed by `source_key`.** The known-source TCP permits and the per-connection stream limit are sized above the app's leg fan-out.
10. **A and P send probe and QUIC datagrams only to sealed host candidates, a `Public` IPv4 rendezvous address from `resolve_public_v4`, and a latched `Public` IPv4 `Peer` address.** The `Peer` must come from the resolved rendezvous address and belong to a punch the side took part in. Each side latches the first valid `Peer` per punch. An authenticated punch admits a source at A but never makes it a target.
11. **Sealing:** an all-zero DH output is rejected; the keys are directional and `Zeroizing`; every seal uses a fresh random 24-byte nonce; the AAD binds the node id, plus the punch id for answers; each message kind has one fixed plaintext length; any AEAD, length, version or cap failure rejects the whole set; P requires the `offer_id` echo; A enforces `OFFER_MAX_AGE`, the runtime's `started_at_ms` and the replay cache.
12. **Secrets:** the direct token is minted from a CSPRNG per binding runtime and travels inside sealed answers and, on TCP, in the plaintext `DirectOpen` preface. It is zeroized on drop, never printed, and compared in constant time. Punch ids and tickets are minted from a CSPRNG at C, per punch and per role, and the device's ticket can never register the gateway's role. `offer_id` is minted from a CSPRNG at P.
13. **C:** a malformed capability or report never closes control; direct state is per punch and has a TTL; (PR1) `MAX_RELAY_NODE_ID_BYTES` is enforced wherever a node id becomes a map key; the direct path never causes a control redial.
14. **C's UDP:** at most one reply per `Register`, only to its sender, never longer than it; only for a live punch, a valid ticket and the role's latched source; IPv4 sources only. `UDP_PUBLIC_ADDR` is configured separately from the relay hostname.
15. **P's probe state:** probes are single-flight per binding; the failure cache is per `(binding, network)` and counts only failed probes and unsolicited early carrier deaths; `network_changed` ignores duplicates and secondary-interface changes and otherwise retires the carrier synchronously; the background barrier suspends the carrier and `.active` re-proves it; a result with a stale epoch is discarded; pair and forget clear carrier state infallibly; a carrier is evicted by closing its connection (`stable_id`), never the endpoint.
16. **P's legs:** every carrier transition invalidates the API leg pool; a failed carrier leg dial falls back to the relay for that leg at once, and only connection-level evidence retires the carrier; while a rotation is in flight, sends are held and the relay leg's `PumpEnded` waits for the rotation's outcome.
17. **Config rules live only in `validate.rs`, and address classes only in `AddressPolicy`.** A never enumerates interfaces when no direct socket or listener is bound.

## Rejected

- **Direct-first dialing with a per-leg budget.** Every leg would pay up to the budget on any network where direct fails, and the relay would stop being the baseline. Relay-first costs nothing when direct fails and one background probe when it works.
- **Migrating the chat leg mid-turn.** The Ephemeral plane is never replayed, so the user would see a hole in the answer.
- **Closing the carrier at `.background`.** A relay chat leg survives a short app switch. Closing the carrier would cut a rotated chat leg mid-reply on every switch and force a new probe on every foreground.
- **Candidates in `ControlHello`.** They go stale. Refreshing them forces control redials, which must never touch live relay legs. They also cannot carry a per-punch freshness proof. Gathering per offer is fresh and costs one interface enumeration.
- **A periodic UDP registration heartbeat.** It would keep state at C per gateway forever, for a mapping that may no longer be the one in use at punch time. On-demand registration observes the mapping at the moment it matters.
- **C verifying a device signature before forwarding.** C would need a device registry. The seal gives A a stronger check for free, and the per-source and per-node limits bound the cost of forwarding garbage.
- **Separate sockets for registration, punches and each QUIC connection.** C would observe a mapping other than the one QUIC uses, and A's port would change under P's carrier.
- **Admitting P by the HTTPS client IP of its POST.** Behind a CDN, or when P reaches C over IPv6, that address is not P's IPv4 mapping. An authenticated punch proves the real source, and C cannot forge it.
- **Symmetric-NAT port prediction, and punching peer-reflexive addresses.** Both are unreliable. Authenticated punches only admit a source; A never targets one. The matrix pins today's `Relay` cells, so adding either later is a deliberate change to the expectations.
- **A persisted route hint.** Every probe must deliver P's candidates anyway, and the probe runs off the critical path.
- **Plain HMAC-authenticated candidates.** They authenticate addresses but show them to C, and they carry no challenge-response freshness.
- **Carrier ranking in `LegDedup`.** P is the only party that knows which chat leg it wants, and it never races two.

## Delivery plan

**PR1: protocol, C and gateway** (inert without the app):

- **Protocol and crypto:** the wire types and constants, including `MAX_RELAY_NODE_ID_BYTES`; `AddressPolicy` with `active()`, `for_tests()` behind `test-support`, `source_key` and `resolve_public_v4`; the `ProbeDatagram` codec. In `device-proto`: `CandidateSealer`, punch tags, `DeviceOffer` and `GatewayAnswer`.
- **`crates/carrier`** (new, `doctest = false`): `DemuxSocket` (GRO segmentation, `try_send` for probe datagrams), the QUIC transport and endpoint config (pinned limits, `grease_quic_bit(false)`, the pinning certificate verifier), `DirectOpen` framing and burst pacing.
  - Its workspace `quinn` dependency carries **no crypto provider**: A enables `rustls-aws-lc-rs`, and P (in the iOS workspace) enables `rustls-ring`.
  - `.github/workflows/ci.yml` widens `IOS_DEPS` and the `ios_native` filter to `crates/(wire|device-proto|model|carrier)/`, so carrier changes run the iOS jobs once the ffi depends on the crate.
- **C:** `POST /direct` with the per-source and per-node budgets; the punch registry and its in-flight rules; control-report parsing; the `ControlRegistry::register` node-id check; the UDP rendezvous (`UDP_PUBLIC_ADDR`, `UDP_BIND_ADDR`, `DEFAULT_UDP_BIND_ADDR`); the edge label; `docker-compose.yml` and `.env.example` (`UDP_PORT`); `DEPLOY.md`.
- **Gateway:**
  - the binding scope in `relay_content::run` and the `RelaySettings` identity fields;
  - the carrier runtime: stable sockets, QUIC endpoint, admission, authenticated-punch verification, punching, opt-in TCP;
  - the outbound `ControlReport` path in `pump_control`, and the capability in the hello;
  - `DeviceLinks`, `GET /v1/mobile/links` and `baybo device status`. The route is registered in the OpenAPI doc, and `docs/openapi.json` is regenerated with `UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync`;
  - config in `crates/config/src/{gateway,validate}.rs`.
- **Tests:** unit tests plus the `relay_e2e.rs` direct cases.
- **Docs:**
  - `companion.md` "Reaching a NAT'd gateway" (content may bypass the relay; pairing stays relay-only);
  - the C can / cannot and Claim 5 delta in `relay-push-security.md`;
  - the routes table and UDP setup in `remote-host/DEPLOY.md`;
  - `config.md`; `gateway.md` (a containerized gateway needs host networking for direct UDP);
  - `cli.md` (the `device` row gains `status`);
  - the NAT hole-punching entry in `docs/roadmap.md`.

**PR2: the app:**

- **ffi:**
  - Generalise `WsStream` to a binary transport in `app/ios/ffi/src/transport/{mod,pump}.rs`, `app/ios/ffi/src/relay/tunnel.rs` and `app/ios/ffi/src/relay/chat.rs` (the chat leg's Noise handshake over a carrier stream). `app/ios/ffi/src/relay/dial.rs` stays WS-only.
  - Add `app/ios/ffi/src/relay/carrier/`: the prober, the QUIC and TCP carriers, the network fingerprint and key, and the failure cache. Add `carrier = { path = "../../crates/carrier" }` to `app/ios/Cargo.toml` `[workspace.dependencies]`, and refresh `app/ios/Cargo.lock` in the same commit.
  - In the supervisor: the active-turn set and the rotation transition, with its commit point and held sends. Invalidate the pool on transitions.
  - In `app/ios/ffi/src/lib.rs`: `network_changed` and `CarrierSink`. The `.background` barrier suspends the carrier and `.active` re-proves it; pair and forget clear carrier state.
  - An in-memory keychain backend for non-iOS builds, behind a `test-support` feature, so the netns client can be seeded with a `PairedRecord`. Today's non-iOS keychain (`app/ios/ffi/src/keychain.rs:336-357`) discards writes.
- **Swift:** `app/ios/App/Core/PathMonitor.swift` and the Settings Connection row. `NSLocalNetworkUsageDescription` is already present in `app/ios/App/Info.plist:33`.
- **Tests and docs:** the netns matrix and its CI job; `app/ios/docs/connection.md` (rotation, suspension and the carrier seam).
- **Before ready:** the owner runs the real-device checklist.

## Related

- [`companion.md`](companion.md) — the iOS companion architecture. Its relay section describes the baseline this doc upgrades from.
- [`relay-push-security.md`](relay-push-security.md) — the Noise IK leg, C's transparency, and the C can / cannot lists this doc extends.
- [`pairing-security.md`](pairing-security.md) — why pairing, including the statics these keys derive from, stays relay-only.
- [`relay-api-tunnel.md`](relay-api-tunnel.md) — the leg pool, its epoch and the `.background` barrier that carrier transitions invalidate.
- [`blob-transfer.md`](blob-transfer.md) — blob legs, which upgrade to carriers but are never migrated in flight.
- [`app/ios/docs/connection.md`](../../../app/ios/docs/connection.md) — the chat supervisor that owns rotation.
- [`sync-protocol.md`](../../sync-protocol.md) — the planes, and why Ephemeral frames forbid mid-turn rotation.
- [`remote-host/DEPLOY.md`](../../../remote-host/DEPLOY.md) — operating C, including the UDP rendezvous.
