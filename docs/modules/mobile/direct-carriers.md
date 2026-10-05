# Direct Carriers for Relay Bindings

**Status: implemented: PR1 (protocol, C and gateway) and PR2 (the app).** The design is delivered as two stacked PRs:

- **PR1: protocol, C and gateway.**
  - Wire types and the address policy (`remote-host/crates/protocol`).
  - Candidate sealing and punch authentication (`crates/device-proto`).
  - A shared QUIC carrier crate (`crates/carrier`).
  - `POST /direct/{relay_node_id}` and the per-punch UDP rendezvous on C.
  - The gateway's carrier runtime, link table and `baybo device status`.

  PR1 is inert until an app speaks it and is covered by gateway↔C e2e tests. C deploys after PR1 merges.
- **PR2: the app.** The iOS ffi prober, carriers and idle chat rotation; the Swift path monitor and Settings row; the netns NAT matrix.

Paths cited with a line number point into this tree. A statement marked (PR1) or (PR2) describes code that PR changed or added. The *Delivery plan* lists both PRs.

A paired phone (P) reaches its gateway (A) through the operator's blind WSS relay (C), even when both sit on the same Wi-Fi. This design keeps that relay as the baseline that always works, and adds **direct carriers**: QUIC over UDP, on a LAN address, an IPv6 address, a public IPv4 address or a hole-punched IPv4 mapping.

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

- The wire and config names keep the `direct` prefix: `/direct/{relay_node_id}`, `ControlSignal::DirectOffer`, `gateway.direct_udp`.
- The code lives under `carrier` names (`crates/carrier`, `crates/gateway/src/channel/carrier/`, and in PR2 `app/ios/ffi/src/relay/carrier/`), so it never sits next to `app/ios/ffi/src/direct/`.
- In prose, a leg on a direct carrier is a **carrier leg**, and its authenticated stream or connection is a **carrier session**.

## Goals

1. Automatic direct connectivity between P and A with **zero router configuration**, so that C leaves the data path whenever the network allows.
2. **Relay first.** No dial ever waits on direct discovery. The user sees the relay's latency at worst, never a stall caused by probing.
3. Content security is unchanged: **every carrier session is a Noise IK session between the paired statics.**
4. C learns as little as possible. Host candidates (LAN addresses, IPv6 addresses, a public IPv4 interface address) cross C only inside sealed sets, so C learns a host candidate only when it is also that side's HTTPS/WSS source address, which C sees anyway. Beyond those source addresses, C sees only the IPv4 mappings it observes itself.
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
| **Carrier** | The transport that legs ride on. The **relay carrier** is C's WSS splice (`/content/join` ↔ `/content/host`). A **direct carrier** is one QUIC connection between P and A. |
| **Candidate** | An address at which a side can be reached. A **host candidate** is an interface address that the side reports about itself: private IPv4, IPv6 ULA, IPv6 GUA, or a public IPv4 address on an interface (a host with no NAT). A **server-reflexive (srflx) candidate** is a side's public IPv4 mapping as observed by C's UDP rendezvous. |
| **Carrier session** | One authenticated leg on a direct carrier: a QUIC bidirectional stream opened with `DirectOpen` and then Noise IK, which P confirms. It runs exactly the relay leg's content responder (Chat) or API tunnel (Api/Blob). |
| **Probe** | One background attempt by P to find a direct carrier. It consists of one `POST /direct`, a candidate exchange, an optional punch, concurrent QUIC connects, and a proof leg. |
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
                 └─────────────────┘                                └─────────────────┘
  every carrier session:  DirectOpen{token, class} ▸ Noise IK (pairing statics) + P's confirmation ▸
                          run_content_session (Chat) | run_tunnel_session (Api, Blob)
```

The ranking below has two uses. It sets the order in which a probe prefers finished attempts, and it gives the display label for each carrier:

| Rank | Carrier (`CarrierKind`) | Reached via | Needs C's UDP rendezvous |
|---|---|---|---|
| 1 | `Lan` | QUIC to a private IPv4 or ULA host candidate | no |
| 2 | `Ipv6` | QUIC to a GUA host candidate; both stateful firewalls are opened by outbound traffic | no |
| 3 | `Ipv4` | QUIC to a public IPv4 host candidate | no |
| 4 | `Ipv4Punched` | QUIC to A's srflx mapping after a punch | yes |
| — | `Relay` | WSS via C | — |

**What C sees**, relative to [`relay-push-security.md`](relay-push-security.md#protected-assets):

| Item | Relay only | With direct carriers |
|---|---|---|
| Source IPs of P and A on HTTPS/WSS | yes | yes (unchanged) |
| P's and A's IPv4 UDP mapping (ip:port) | no | **yes**, observed by C itself during a punch |
| That a direct attempt happened, and its timing and outcome at C | no | **yes** |
| Host candidates and the direct token | — | **no**: they are sealed. A host candidate that is also that side's HTTPS/WSS source address (for example the GUA P posts from, or the address of a gateway without NAT) is visible only as that source, as it was without direct carriers |
| A's QUIC certificate | — | not sent to C; C can fetch it by naming its own address as P's srflx (see *Security*), which grants nothing |
| How many candidates each side has | — | **no**: each sealed message kind has one fixed plaintext length |
| Lengths and timing of traffic on a direct carrier | — | **no**: the carrier bypasses C |

## Candidates

### Address policy: one home

All address classification lives in one type, `AddressPolicy` in the protocol crate's `relay` module (`remote-host/crates/protocol/src/relay/address.rs`, PR1). A, C and P all call it. Nothing else re-derives an address class; the config validator does not either (see *Config*).

| Class | IPv4 | IPv6 |
|---|---|---|
| `Lan` | 10/8, 172.16/12, 192.168/16, 100.64/10, 169.254/16 | fc00::/7 (ULA) |
| `Public` | Anything not excluded below | 2000::/3, minus the exclusions |
| excluded | 0/8, 127/8, 192.0.0/24, 192.88.99/24, 198.18/15, documentation ranges, multicast, ≥240/4, broadcast | ::/128, ::1, fe80::/10 (it needs a scope id), multicast, 2001:db8::/32, 2001:2::/48, 2001::/32 (Teredo), 2002::/16 (6to4) |

IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) are canonicalised to IPv4 before they are classified.

The protocol crate also owns the derived questions that would otherwise be answered at each call site:

- `AddressPolicy::source_key(ip)` is the key of every per-source cap at A: the IPv4 address, or the IPv6 /64. `AddressPolicy::client_key(ip)` is the key of C's per-client budgets: the IPv4 address, or the IPv6 /48.
- `UdpRendezvous::resolve_public_v4(&policy)` resolves a rendezvous address. A literal must be `Public` IPv4. A hostname is looked up, only its IPv4 (A-record) results are used, and every one of them must be `Public`. It blocks on the system resolver, so async callers run it on a blocking thread. A and P both call it.
- `UdpRendezvous::normalize_address(value, &policy)` is C's startup normalisation of `UDP_PUBLIC_ADDR`: a `Public` IPv4 literal or a lowercase LDH hostname whose last label is not all digits, plus a non-zero port.

Callers obtain the policy from `AddressPolicy::active()`. When the protocol crate is built with its `test-support` feature, `active()` returns `AddressPolicy::for_tests()`, which also classifies 198.18/15, 2001:2::/48 and 127/8 as `Public`, and `::1` as `Lan`. A release build does not enable the feature and cannot select that policy. The in-process e2e tests and the netns matrix build with it.

### Sockets

Each side holds **one UDP socket per address family** while its carrier machinery is active:

- **A** binds `gateway.direct_udp.ipv4_bind` (default `0.0.0.0:0`) and `ipv6_bind` (default `[::]:0`, with `IPV6_V6ONLY` set) once per binding runtime. It binds a family again only when its first bind fails (a fixed port still held by the previous runtime's draining connections, or by another process), every `REBIND_DELAY`, or when its receive errors persist. The port is ephemeral but stable for the life of the runtime.
- **P** binds `0.0.0.0:0` and `[::]:0` (also with `IPV6_V6ONLY` set) when a probe starts. The carrier the probe wins keeps them, and they close with it; a probe that finds nothing closes them. A P socket therefore never outlives the carrier it serves.

On each side, the UDP rendezvous registration, the punch datagrams and QUIC all share that one socket. This is required, not a convenience: the IPv4 mapping that C observes is the mapping QUIC uses. `crates/carrier` (PR1) wraps the socket in a `DemuxSocket`. `DemuxSocket::bind(address)` binds it, setting `IPV6_V6ONLY` on an IPv6 socket, and returns it with the queue its probe datagrams arrive on:

- The socket is built on `quinn_udp::UdpSocketState`, so replies leave from the local address the peer targeted (`IP_PKTINFO` / `IPV6_RECVPKTINFO`). This matters on hosts with several IPv6 addresses.
- The socket carries exactly one QUIC endpoint, built by `DemuxSocket::quic_endpoint`, and that endpoint's driver is the socket's only reader. Probe datagrams therefore arrive once the endpoint exists, and a second endpoint on the same socket is refused. `DemuxSocket` does not implement `quinn::AsyncUdpSocket` itself: its quinn side is a private adapter that only `quic_endpoint` builds, so no caller can hand the socket to quinn another way.
- On Linux quinn-udp enables UDP GRO, so one receive buffer can hold several datagrams of one flow. The demux splits every buffer into `stride`-sized segments and classifies each segment on its own. Probe segments are decoded one at a time; the QUIC segments quinn may see are packed to the front of the buffer with their stride kept, and a buffer left with none reaches quinn empty.
- A segment whose first byte has `0x80` or `0x40` set (`QUIC_FIRST_BYTE_MASK`) is QUIC. Everything else is a probe datagram (see *Rendezvous and punch datagrams*). The protocol crate owns this rule as `is_probe_datagram`.
- A QUIC segment goes to quinn unless quinn would answer it before an `Incoming` exists, where A's admission cannot suppress the answer. Two kinds of long-header packet qualify, and the demux drops both: one naming a version other than QUIC v1, which quinn answers with a Version Negotiation packet whatever its size, and a v1 Initial whose destination connection ID is shorter than 8 bytes, which quinn answers with a CONNECTION_CLOSE. Both sides speak only QUIC v1 (`EndpointConfig::supported_versions`), and every Initial they exchange names a connection ID of at least 8 bytes (RFC 9000 §7.2, and A's own IDs after a Retry), so neither kind is ever legitimate. quinn's remaining answers before admission (a stateless reset, or a CONNECTION_CLOSE for a Retry token that has expired or comes from another address) need a connection ID or Retry token that A issued, and each is smaller than the packet that caused it.
- A decoded probe datagram goes on a bounded queue (`PROBE_QUEUE_CAPACITY`) as a `ReceivedProbe`: the datagram, its canonicalised source, and the local address it targeted. A malformed one, or one that finds the queue full, is dropped, as UDP loss would drop it.
- Probe datagrams are sent with `DemuxSocket::send_probe(datagram, destination, source)`, which uses `UdpSocketState::try_send`, so a send error reaches the caller. The optional `source` picks the local address, which A needs to punch from each of its host addresses. QUIC transmits go through `send`, which logs send errors and drops them. P relies on this to detect a refused Local Network permission (see *The prober*).
- A receive error returned to quinn would end the endpoint's driver, and every connection on the socket with it. The demux therefore absorbs a receive error and waits `SOCKET_RECV_BACKOFF` before reading again. It reports the current streak (`consecutive_recv_errors`), which A uses to judge a persistent error.
- Every endpoint sets `EndpointConfig::grease_quic_bit(false)`, so it never advertises the `grease_quic_bit` transport parameter (RFC 9287), and its peer therefore never sends it a packet with the fixed bit clear. Each side's own setting is what keeps its own demux unambiguous. `DemuxSocket::quic_endpoint` is the only place a carrier endpoint is built, so no endpoint can omit the setting.

This UDP traffic never goes through the gateway's egress proxy, which governs HTTP(S) and WebSocket traffic only, the relay dials included ([`config.md`](../config.md)).

Neither side needs a port-forward rule, and a stateful firewall on either side is opened by that side's own traffic: **every side sends to a candidate before it expects to receive from it.** The one path that breaks this is a source A admits only through an authenticated punch (see **Punch authentication**), such as a phone behind NAT reaching a public-IPv4 gateway with no rendezvous: A never sends to that source, so a stateful host firewall on A drops it unless it accepts inbound UDP on the `direct_udp` port. That needs a fixed `direct_udp.ipv4_bind` port; with the default ephemeral one, such a gateway is reached through the rendezvous or stays on the relay.

### Gathering

**A** enumerates its interface addresses once per accepted offer. It does so only while at least one direct socket is bound, so a gateway with direct carriers disabled never walks its interfaces.

- It reads addresses and interface flags through `getifaddrs`, and each IPv6 address's own flags from `/proc/net/if_inet6` on Linux and through `SIOCGIFAFLAG_IN6` on Apple platforms. The enumeration is `carrier::interfaces` (`crates/carrier/src/interfaces.rs`), which A and P share; it reports a temporary IPv6 address apart from an unusable one, because the two sides treat them differently. A skips IPv6 addresses flagged temporary, deprecated, tentative or duplicate (DAD failed), and an IPv6 address whose flags it cannot read. A failed enumeration reads as no address, so the answer carries no host candidate.
- It keeps addresses on UP+RUNNING interfaces whose class is `Lan` or `Public`.
- It skips interfaces whose names start with one of `VIRTUAL_INTERFACE_PREFIXES`: container bridges and veths (`docker`, `br-`, `veth`, `virbr`, `cni`, `lxc`) and VPN tunnels (`tailscale`, `wg`, `tun`, `utun`, `zt`).
- It keeps at most one GUA per /64 and at most `MAX_GATEWAY_GUAS` GUAs, `MAX_GATEWAY_ULAS` ULAs and `MAX_GATEWAY_IPV4_HOSTS` IPv4 addresses, ranked ULA/private < GUA < public IPv4.
- Each address is paired with the port of that family's stable socket.

**P** gathers from the primary interface of the currently satisfied `NWPath`:

- The primary is the first physical interface (Wi-Fi, wired or cellular) in
  system preference order whose type `NWPath.usesInterfaceType` reports as
  used, including beneath a VPN. With none, P uses the first used interface
  or leaves the primary unset. A leading VPN tunnel must not hide active Wi-Fi, but an unused
  Wi-Fi interface must not enable LAN probing on a cellular path. This only
  selects candidate addresses; sockets continue to obey system routing.
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

Each side has its own sealer type in `crates/device-proto/src/candidates.rs` (PR1): P derives a `DeviceSealer::derive(own_secret, gateway_public)` and A a `GatewaySealer::derive(own_secret, device_public)`. Both hold the same three keys, named by direction (`k_p2a`, `k_a2p`, `k_punch`), and the type fixes which of them the side may use (see **Verbs**). A low-order peer key fails either derivation with `ProtoError::WeakPeerKey`. The key properties are:

- The sealing keys are directional: a sealed set can never be reflected back to its sender as if the peer had sent it.
- The shared secret and the three keys are held in `Zeroizing`. The PRK exists only inside a transient `Hkdf` value, which hkdf 0.12 does not wipe, and that value is dropped right after the expands. snow's X25519, which computes `shared`, keeps its own copy of the secret and does not wipe it on drop, so `derive` overwrites that copy first. Each punch tag's HMAC state is transient and unwiped, like the `Hkdf`. `StaticKeypair::secret()` (`crates/device-proto/src/noise.rs:53`) returns a plain copy, and each call site wraps that copy in `Zeroizing` immediately.
- A holds one `GatewaySealer` per binding runtime. P derives a `DeviceSealer` per probe and drops it afterwards.
- The keys have **no forward secrecy**. They protect addresses and a per-runtime token, not content, and anyone who compromises a static secret can already impersonate that endpoint.

**AEAD.** Sealing uses **XChaCha20-Poly1305** (`chacha20poly1305 0.10`, already a `device-proto` dependency) with a **fresh random 24-byte nonce** per seal.

- The key lives as long as the binding, and both a phone and a restarting gateway process seal under it. Random 192-bit nonces need no persisted counter and make a collision negligible.
- Associated data binds the context:
  - offer: `"baybo/direct/offer/v1" ‖ u64be(len(relay_node_id)) ‖ relay_node_id`
  - answer: `"baybo/direct/answer/v1" ‖ u64be(len(relay_node_id)) ‖ relay_node_id ‖ punch_id (16 bytes)`

**Plaintext.** The plaintext is `u16be(len) ‖ msgpack(body) ‖ zero padding` to **one fixed length per message kind**: `SEALED_OFFER_PLAINTEXT_LEN` for offers and `SEALED_ANSWER_PLAINTEXT_LEN` for answers.

- Each length holds the largest body the caps allow (every candidate IPv6), so the ciphertext length does not reveal the candidate count. At their longest encodings those bodies are 402 and 525 bytes. Once base64-encoded, the two sealed kinds are 736 and 908 bytes. A test pins all four figures. Compile-time asserts, which count the base64 length with `base64::encoded_len`, keep both kinds within `MAX_SEALED_CANDIDATES_BYTES`.
- The body is msgpack with named fields. It is sized first and then encoded straight into the zeroed fixed-length buffer, so no growable copy of it exists.
- `seal` checks the version and the caps, and fails if a body does not fit. `open` rejects a ciphertext of any other length before the AEAD. After the AEAD it rejects a declared length that overruns the plaintext, padding that is not zero, and a body whose msgpack value does not end exactly at the declared length.
- Fixed-size byte fields are encoded with `serde_bytes`, so their encoded length does not depend on their values.
- The plaintext buffer is `Zeroizing`, because an answer carries the direct token, and the token is decoded straight into `DirectToken`.

**Verbs.** Each sealer type has one method per step of its own side, so no caller composes the rules itself and neither side holds the other's verbs:

- `DeviceSealer` (P) has `seal_offer(relay_node_id, &DeviceOffer)`, `open_answer(relay_node_id, &punch_id, &offer_id, &SealedCandidates)` and `punch_tag(&offer_id, seq)`. `open_answer` enforces the `offer_id` echo.
- `GatewaySealer` (A) has `open_offer(relay_node_id, &SealedCandidates)`, `seal_answer(relay_node_id, &punch_id, &GatewayAnswer)` and `verify_punch_tag(&offer_id, seq, &PunchTag)`. Freshness and replay stay with A.
- `GatewaySealer` cannot mint a punch tag, so A's own punches cannot carry a valid one (see **Punch authentication**).
- Every refusal is a `ProtoError`: `Aead` when the AEAD open fails, `Candidates(CandidateRejection)` for the size, encoding, length, framing, version, cap and echo rules, and `Codec` for a malformed body. No `CandidateRejection` variant carries a candidate, the token or key material.

```rust
// crates/device-proto/src/candidates.rs (PR1)
pub const CANDIDATE_SET_VERSION: u8 = 1;
pub const OFFER_ID_LEN: usize = 16;
pub const CERT_HASH_LEN: usize = 32;

/// 16 CSPRNG bytes, minted by P per offer.
pub struct OfferId([u8; OFFER_ID_LEN]);
/// SHA-256 of A's QUIC certificate DER: `CertHash::of_certificate(der)`. Compared in constant time.
pub struct CertHash([u8; CERT_HASH_LEN]);

/// P → A, sealed under k_p2a. `DeviceOffer::new(issued_at_ms, udp)` sets `v` and mints `offer_id`.
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
}
```

**Freshness and replay.** C can store and replay anything it forwards, so both directions are protected:

- **A** accepts an offer only when all of these hold:
  - `|now − issued_at_ms| ≤ OFFER_MAX_AGE`;
  - `issued_at_ms` is not earlier than the gateway process's `started_at_ms`, taken when its relay-content manager starts;
  - its `offer_id` is not in the replay cache. The cache belongs to the gateway process, not to a runtime (`CarrierProcess` in `crates/gateway/src/channel/carrier/runtime.rs`, with the QUIC certificate), so a Reconfigure, which starts a new runtime, never empties it. It holds up to `MAX_REPLAY_ENTRIES` ids for `2 × OFFER_MAX_AGE`, including that window's last millisecond, so an id outlives every moment its offer is fresh. When it is full, A declines the new offer (`declined:over_cap`) instead of evicting an unexpired id.

  A declined offer gets no punch. A process restart empties the cache, and the start-time rule then compares P's clock with A's: it declines every offer issued before the restart by a P whose clock agrees with A's or lags it, and a P whose clock lags A's loses at most `OFFER_MAX_AGE` of probes after a restart, which is one backoff step. When P's clock runs ahead of A's by up to `OFFER_MAX_AGE`, an offer C captured within that lead before a gateway process restart passes both rules once, and A treats it as a genuine offer (see *C can*).
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

- **A's hello.** (PR1) `ControlHello` carries `direct: Option<DirectCapability>`. A sends it whenever its carrier runtime is active, that is, serves at least one configured UDP family, whether or not its first bind succeeded. The capability names the families the runtime serves, not what is bound at that instant, so it is the same in every hello of a binding scope and never goes stale while a family is being bound again (a family still retrying its first bind, or a rebind after persistent receive errors): an offer that arrives meanwhile is declined as `unbound`, or answered without that family's candidates. `udp: true` means A serves an IPv4 UDP family and will register on demand.
- **How C decodes it.** C closes control on an unparseable hello (`remote-host/crates/relay/src/serve.rs:759-772`), so the capability field is decoded **leniently**: any decode failure yields `None` and is logged once. A malformed capability must not cost the gateway its relay.
- **Unknown versions.** An unknown `version` is treated as not direct-capable, and control stays up. `ControlHello::supported_direct()` is that predicate: the capability, when it is present and of `DIRECT_PROTOCOL_VERSION`.
- **What C sends.** C sends `DirectOffer` only to a control connection whose hello carried a supported capability. A gateway without direct carriers therefore never receives it. If one ever did, it would warn and skip the frame, as every gateway does with a control signal it cannot parse (`crates/gateway/src/relay/mod.rs:200-209`).

### Sequence

1. **P posts its offer.** P binds its probe sockets, gathers its host candidates, seals a `DeviceOffer`, and sends `POST /direct/{relay_node_id}` with the `x-remote-api-key` header and a `DirectOfferRequest` body.
2. **C admits and routes.** C admits the request the same way it admits every relay route:
   - the per-IP token bucket (`remote-host/crates/edge/src/ip_limit.rs`), then
   - `require_admitted` (`remote-host/crates/relay/src/serve.rs:549`).

   A body that is not a `DirectOfferRequest`, or whose offer exceeds `MAX_SEALED_CANDIDATES_BYTES`, gets `400`; one over `MAX_DIRECT_OFFER_BODY_BYTES` gets `413`. C then looks up the node's control entry. Each of the following produces the same opaque `404 no direct route`: a node id over `MAX_RELAY_NODE_ID_BYTES`, no entry, an owner-key mismatch, or no supported capability. C then reserves a slot on the node's control channel: a full channel answers `503` with `Retry-After: CONTROL_BUSY_RETRY_AFTER`, and a control connection that has just ended answers `504`. Next come the per-(node, client) rate, the per-node ceiling, the in-flight cap and the per-client punch cap, each answering `429` with `Retry-After`, and then `MAX_PENDING_PUNCHES`, answering `503` with `Retry-After`. A refused offer consumes no budget, whichever check refused it. Every response on the route carries `Cache-Control: no-store`, including the per-IP limiter's and admission's.
3. **C mints the punch.** C mints a `PunchId`. If `UDP_PUBLIC_ADDR` is configured and the capability has `udp`, C also mints two `RendezvousKey`s, one per role. C sends this signal in the slot it reserved on the node's control channel:

   `ControlSignal::DirectOffer { punch_id, offer, register }`

   `register` carries the rendezvous address and the gateway-role key.
4. **A handles the offer.** A opens the offer and runs the freshness and replay checks. If A already holds `MAX_INFLIGHT_PUNCHES_PER_NODE` live punches, the new offer supersedes the oldest: only P can seal an offer, and P runs one probe at a time, so a new genuine offer means P has abandoned the old one. A then creates the punch's **allowed-IP set** from P's host candidate IPs. It gathers its own candidates and seals a `GatewayAnswer` bound to `punch_id`. It then replies on the control WebSocket with:

   `ControlReport::DirectAnswer { punch_id, answer }`

   On any rejection it replies `DirectDeclined { punch_id }` and does nothing else.
5. **A punches and registers.** A sends `PUNCH_BURST` punch datagrams to every P host candidate, from each of its own host addresses in that family. It sends the highest-ranked pairs first and stops at `MAX_PUNCH_DATAGRAMS_PER_OFFER`: every pair gets a whole burst, and one burst of the budget is kept for the `Peer` mapping. If `register` is present and A has an IPv4 socket, A resolves the rendezvous address with `resolve_public_v4`; if that fails, or the address is one of A's own interface addresses, A skips registration. Otherwise it sends `Register{punch_id, Gateway, tag}`, tagged under the gateway-role key, from its IPv4 socket every `REGISTER_RETRY_INTERVAL` until it holds a `Peer` or `PEER_WAIT` elapses. A `Registered` reply confirms the `Register` and does not end the loop.
6. **C answers P's POST.** C waits up to `DIRECT_ANSWER_TIMEOUT` for the report:
   - `DirectAnswer` → `200 DirectOfferResponse { punch_id, answer, rendezvous }`, where `rendezvous` carries the device-role key, with `Cache-Control: no-store`;
   - `DirectDeclined` → `404`;
   - no report within the timeout, or control gone → `504`.
7. **P checks the answer and starts connecting.** P opens the answer and checks the `offer_id` echo. To each A host candidate it may dial (see *What P dials*), P sends an authenticated punch and then starts a QUIC connect; the rest of that burst follows at `PUNCH_INTERVAL`. An Initial that overtakes its punch is ignored by A and retransmitted by quinn.
8. **P registers.** If `rendezvous` is present, P resolves it with `resolve_public_v4`. If that fails, the `ipv4_punched` tier is `not_offered` and P sends no `Register`. Otherwise P registers from its IPv4 socket exactly as A does.
9. **C replies to each `Register`.** For a live punch and a `Register` whose tag verifies under that role's key, C latches the first source it observes for that role, and it drops a later `Register` for the same role from any other source. It replies once per `Register`, to the sender only, tagged under the same role key:
   - `Peer{punch_id, srflx}`, carrying the other role's latched mapping, once both roles are observed;
   - `Registered{punch_id}` before that.
10. **Both sides check `Peer` and punch.** Each side accepts `Peer` only when it comes from the resolved rendezvous address, carries its own `punch_id`, verifies under its role key, and names a `Public` IPv4 address that is none of the side's own interface addresses. Each side **latches the first valid `Peer` per punch**: an identical later one is ignored, and one naming a different address is dropped and logged (`peer_conflict`). A adds the srflx IP to the allowed set for the rest of `PUNCH_TTL`. Both sides then punch the srflx (`PUNCH_BURST` at `PUNCH_INTERVAL`), and P starts a QUIC connect to A's srflx. P's own punches and retransmitted Initials open P's NAT, and A's punches open A's NAT.
11. **P picks a winner.** When the first QUIC handshake completes, P waits up to `TIER_GRACE` for a better-ranked attempt still in flight. It keeps the best one and closes the rest. The probe ends on the relay when no QUIC handshake has completed within `PROBE_BUDGET`, or every QUIC attempt has already errored.
12. **P proves the carrier.** P opens one session on the winning carrier (`DirectOpen{token, class: Api}`, then Noise IK and its confirmation). A successful handshake proves the carrier, and the resulting API leg is parked in the leg pool. Only then does the carrier become live on P.

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

- **Configuration.** `crates/carrier` builds both sides' configuration: `server_config(&ServerIdentity, provider)` for A and `client_config(pinned: CertHash, provider)` for P. Each side passes in its own rustls `CryptoProvider`; the crate names none.
- **ALPN.** Both sides set `DIRECT_QUIC_ALPN`.
- **Certificate.** A's certificate is a per-process `rcgen` self-signed certificate for `DIRECT_QUIC_SERVER_NAME` (`ServerIdentity::generate`, run when the first binding runtime has a socket to bind and kept for the life of the process in its `CarrierProcess`, beside the offer replay cache; A seals `ServerIdentity::cert_hash` into every answer). P's `ServerCertVerifier`:
  - compares the SHA-256 of the end-entity certificate with `quic_cert_sha256` in constant time (`CertHash::of_certificate` and `CertHash` equality);
  - rejects any intermediates;
  - keeps rustls's `verify_tls13_signature` against the presented certificate;
  - allows TLS 1.3 only.

  Neither side resumes a TLS session: A sends no session tickets and P disables resumption, so every handshake checks the pin. QUIC's TLS is therefore authenticated and hides the `DirectOpen` token even from an active LAN attacker, but it still grants nothing: **Noise IK is the authentication for every session.**
- **Transport.** Both sides refuse unidirectional streams and datagrams, and share the keep-alive and idle timeout. A additionally pins its limits (see *Gateway (A)*). P grants A no bidirectional stream either, since A never opens one.
- **Framing.** A `DirectOpen` is a u32-BE-length-prefixed JSON record capped at `MAX_DIRECT_FRAME_BYTES`, and it is the first thing on every stream. The Noise frames that follow use the same framing. `crates/carrier` owns it: `write_frame` and `write_direct_open`, and a `FrameReader` whose `next_frame` never reads past the frame it assembles and is cancel safe, so a pump can read inside a `select!`. A stream that ends between frames reads as a clean end, and one that ends inside a frame is a truncation. A declared length over the cap is refused before anything is allocated for it, and a malformed preface is refused without echoing its bytes. An error ends the stream: the reader has lost its place, so every later `next_frame` fails with `FrameError::Poisoned`. The frame being assembled may be the preface, so the reader keeps it in `Zeroizing` and its `Debug` prints only lengths.
- **Handshake confirmation.** Every carrier session's Noise IK handshake is confirmed: right after P reads msg2, before anything else, it sends one transport message with an empty payload. A counts the session as authenticated (the `authenticated` hook, `LegDedup`, the device's `last_seen`) only once that message decrypts, within the responder's `HANDSHAKE_TIMEOUT` (10 s, `crates/gateway/src/channel/device_content.rs`); a confirmation that does not decrypt, or carries a payload, refuses the session. Noise IK's msg1 carries no replay protection, and the confirmation is encrypted under keys that only the initiator that wrote msg1 holds, so a replayed msg1 gets A's msg2 and nothing more. The seam declares it: carrier sinks set `BinarySink::CONFIRMS_HANDSHAKE`, and the relay leg, whose msg1 crosses only TLS to C, keeps the two-message handshake. C can still replay a relay leg's msg1, so a relay chat leg installs its `LegDedup` only once its first transport message from P decrypts, which a replayed msg1 never produces (see *Dedup*).

### Wire types (PR1, `remote-host/crates/protocol/src/relay.rs`)

```rust
pub const DIRECT_OFFER: &str = "/direct/{relay_node_id}";
/// The offer URL on the relay's HTTP(S) origin: a `wss://` / `ws://` base maps to `https://` / `http://`.
pub fn direct_offer_url(base: &str, relay_node_id: &str) -> String;
pub const DIRECT_PROTOCOL_VERSION: u16 = 1;
pub const MAX_RELAY_NODE_ID_BYTES: usize = 128;

/// 16 CSPRNG bytes, minted by C per punch; 32 lowercase hex on the JSON wire.
pub struct PunchId([u8; 16]);
/// 32 CSPRNG bytes, minted by C per punch and role; 64 lowercase hex on the JSON wire.
/// Delivered only over TLS, never on the UDP rendezvous. Zeroized on drop,
/// redacted by `Debug`, compared in constant time.
pub struct RendezvousKey(Zeroizing<[u8; 32]>);
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
/// `MAX_SEALED_CANDIDATES_BYTES` caps `n` and `enc` together.
#[derive(Serialize, Deserialize)]
pub struct SealedCandidates {
    pub n: String,
    pub enc: String,
}

#[derive(Serialize, Deserialize)]
pub struct UdpRendezvous {
    pub address: String,              // normalized `host:port` (UDP_PUBLIC_ADDR)
    pub key: RendezvousKey,           // the recipient's role only
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

/// First framed record on every direct QUIC stream.
#[derive(Serialize, Deserialize)]
pub struct DirectOpen { pub token: DirectToken, pub class: LegClass }
```

### Rendezvous and punch datagrams

Probe datagrams are binary, and their first byte never looks like QUIC:

```
byte 0   PROBE_DATAGRAM_MAGIC = 0x0b          (bits 0x80 and 0x40 clear)
byte 1   kind
  1 Register     punch_id[16] role[1] tag[16]          zero-padded to REGISTER_DATAGRAM_LEN (role: 1 gateway, 2 device)
  2 Registered   punch_id[16] tag[16]
  3 Peer         punch_id[16] ipv4[4] port[2] tag[16]
  4 Punch        seq[2] tag[16]                        (P → A: authenticated; A → P: random tag)
```

The codec is `ProbeDatagram::{encode, decode}`, in the protocol crate. Integers are big-endian, and decoding requires each kind's exact length and zero padding.

**Rendezvous tags.** The tag of a `Register`, `Registered` or `Peer` is `HMAC-SHA256(role key, "baybo/direct/rendezvous/v1" ‖ every byte before the tag)[..RENDEZVOUS_TAG_LEN]`, under the key of the role that sends the `Register` or receives the reply. `RendezvousKey::{register, registered, peer}` mint the three datagrams and `RendezvousKey::verifies` checks one in constant time. The key reaches each role only over TLS (A's in its `DirectOffer`, P's in its POST response), so a party that can only watch or forge UDP between a side and C can neither forge a reply nor a `Register`. Each side's registration loop and its `Peer` rule (sequence steps 5, 8 and 10) live once in `crates/carrier` as `carrier::rendezvous::{Registration, PeerLatch}`, which A and P both run. C replies at most once per `Register`, only to its sender, and `REGISTER_DATAGRAM_LEN` exceeds every reply, so **C never emits more datagrams or bytes than it receives.**

**Punch authentication.** Every punch P sends carries `seq`, a per-probe counter, and `tag = HMAC-SHA256(k_punch, offer_id ‖ u16be(seq))[..16]`.

- A checks a tag against each of the binding's live punches in constant time and accepts each `(punch, seq)` once.
- On a match, A adds the datagram's source IP to that punch's allowed-IP set, up to `MAX_PRFLX_SOURCES_PER_PUNCH` sources. This admits P's real source even when no rendezvous observed it, for example a phone behind NAT reaching a public-IPv4 gateway.
- A never sends to an address it learned this way.
- A's own punches carry a random tag, since `GatewaySealer` has no tag-minting verb, and P drops every `Punch`.

## Constants

Every value is in the code, at the place the *Where* column names: the protocol, C, A and carrier values since PR1, P's (`app/ios/ffi/src/relay/carrier/` and the chat supervisor) since PR2.

| Value | Where | Buys | Costs |
|---|---|---|---|
| `MAX_UDP_HOST_CANDIDATES = 8` | protocol | Decode cap for either side's sets | P hosts with many GUAs lose the lowest-ranked ones |
| `MAX_GATEWAY_GUAS = 2`, `MAX_GATEWAY_ULAS = 1`, `MAX_GATEWAY_IPV4_HOSTS = 2` | A | One stable GUA per /64; room for a private and a public IPv4 address | Hosts with more addresses lose the lowest-ranked ones |
| `VIRTUAL_INTERFACE_PREFIXES` | A | Container and VPN interfaces are never offered | A carrier over a VPN overlay is not attempted |
| `SEALED_OFFER_PLAINTEXT_LEN = 512`, `SEALED_ANSWER_PLAINTEXT_LEN = 1024` | protocol | Ciphertext length is constant per kind; each holds the full-cap all-IPv6 body (402 and 702 bytes at worst), pinned by a test | 1.5 KiB of sealed data per probe |
| `MAX_SEALED_CANDIDATES_BYTES = 2048`, `MAX_DIRECT_OFFER_BODY_BYTES = 4 KiB` | protocol / C | Bounded parsing | — |
| `MAX_RELAY_NODE_ID_BYTES = 128` | protocol | Bounds every map keyed by node id | — |
| `OFFER_MAX_AGE = 120s`, `MAX_REPLAY_ENTRIES = 64` | A | Tolerates ordinary phone/gateway clock skew; the replay window is `2 × OFFER_MAX_AGE` | Skew over 2 min disables direct (logged as `declined:stale`) |
| `DIRECT_ANSWER_TIMEOUT = 3s` | C | A answers in milliseconds; bounds a parked POST | A wedged gateway costs a probe 3 s of background time |
| `CONTROL_BUSY_RETRY_AFTER = 1s` | C | `Retry-After` on the `503` for a full control channel | — |
| `PUNCH_SWEEP_INTERVAL = 5s` | C | Expired punches are reclaimed and their end logged promptly; lookups ignore them regardless | — |
| `PUNCH_TTL = 20s`, `MAX_DATAGRAMS_PER_PUNCH = 64` | C; `PUNCH_TTL` is in protocol, shared with A's allowed-IP set | Per-punch state only; covers both sides' full registration loops; only verified `Register`s from a role's latched source spend the datagram budget | A probe slower than 20 s loses its rendezvous |
| `REGISTRATION_GRACE = 3s` | C | An answered punch P never registers for ends instead of holding a slot for `PUNCH_TTL` | P must register within 3 s of its `200` |
| `REGISTER_RETRY_INTERVAL = 250ms`, `PEER_WAIT = 5s`, `REGISTER_DATAGRAM_LEN = 64`, `RENDEZVOUS_TAG_LEN = 16` | `REGISTER_RETRY_INTERVAL` in carrier (`carrier::rendezvous`); the rest in protocol, `PEER_WAIT` shared with C, which keeps a paired punch that long after pairing | Registration survives the loss of any datagram, including a `Peer` | ≤ 20 datagrams of 64 B per side per punch |
| `PUNCH_BURST = 5`, `PUNCH_INTERVAL = 200ms` | carrier (`PunchBurst`), used by A and P | Covers the skew between the two `Peer` deliveries; later bursts pass the NAT opened by the earlier ones | See the caps below |
| `MAX_PUNCH_DATAGRAMS_PER_OFFER = 256`, `MAX_PRFLX_SOURCES_PER_PUNCH = 4` | A | Bounds A's fan-out and what authenticated punches can admit | Lowest-ranked pairs are not punched |
| `MAX_PUNCH_DATAGRAMS_PER_PROBE = 64` | P | Bounds P's fan-out; one QUIC attempt per remote candidate | — |
| `PROBE_BUDGET = 10s`, `TIER_GRACE = 300ms` | P | The whole probe runs in the background; a later LAN success beats an earlier srflx one | None user-visible |
| `DIRECT_QUIC_KEEP_ALIVE = 10s`, `DIRECT_QUIC_IDLE_TIMEOUT = 45s` | carrier | Keepalive under common UDP NAT timeouts; the idle timeout equals the pump's `INBOUND_LIVENESS_TIMEOUT` (`app/ios/ffi/src/transport/pump.rs:28`), pinned by an ffi test | One small packet every 10 s |
| `DIRECT_QUIC_ALPN = "baybo-direct/1"`, `DIRECT_QUIC_SERVER_NAME = "baybo-direct"` | carrier | A fixed identity for the self-signed certificate | — |
| `MAX_DIRECT_FRAME_BYTES = 65535` | carrier | One frame carries one Noise message, whose maximum this is | — |
| `QUIC_STREAM_RECEIVE_WINDOW = 256 KiB`, `QUIC_CONNECTION_RECEIVE_WINDOW = 1 MiB` | carrier | Bounds what an admitted, unauthenticated peer can make A buffer | Upload throughput per stream ≤ window / RTT (≈ 5 MB/s at 50 ms) |
| `PROBE_QUEUE_CAPACITY = 256` | carrier | Received probe datagrams wait for their owner in bounded memory | A burst beyond it is dropped, as UDP loss would drop it |
| `DIRECT_OPEN_DEADLINE = 1s`, `FIRST_STREAM_DEADLINE = 3s` | A | A silent stream or connection cannot hold a slot; the first-stream deadline starts at `Incoming::accept()` and covers the handshake | — |
| `MAX_QUIC_CONNECTIONS = 8`, `MAX_QUIC_CONNECTIONS_PER_SOURCE = 4`, `MAX_STREAMS_PER_CONNECTION = 32` | A; `MAX_STREAMS_PER_CONNECTION` is in carrier, which pins it in A's `TransportConfig` | One carrier plus the losing probe attempts (≤ 3 per P source); streams cover the app's fan-out | — |
| `REBIND_AFTER_RECV_ERRORS = 6` | A | A receive-error streak this long (about 8 s of backed-off failures, checked every second) is persistent and rebinds the family's socket | The family's QUIC connections close on a rebind |
| `INCOMING_LOG_INTERVAL = 60s` | A | `quic_incoming` is a counter line, not one line per packet | — |
| `REBIND_DELAY = 2s`, `CARRIER_DRAIN_GRACE = 4s` | A | A family whose bind failed retries; a stopping runtime waits for its connections to drain and its sockets to close, which covers the 3×PTO drain of a connection closed mid-handshake (about 3 s at quinn's 333 ms initial RTT) | A Reconfigure or shutdown waits up to 4 s while connections drain |
| `SOCKET_RECV_BACKOFF = 250ms · 2ⁿ, max 4s` | protocol (`socket_recv_backoff`), used by C and by carrier's `DemuxSocket` (A and P) | A receive error never tears down the runtime or C's HTTP/WSS | — |
| `UDP_SOURCE_WARN_INTERVAL = 60s` | C | Source-rewriting deployments stay visible without log floods | — |
| `DEFAULT_UDP_BIND_ADDR = 0.0.0.0:7777` | C | IPv4 rendezvous by default | — |
| `DIRECT_LEG_DIAL_TIMEOUT = 3s`, `CARRIER_DIAL_COOLOFF = 30s` | P | Bounds a blackholed carrier to one slow API dial; a stream-level failure pauses new carrier legs without dropping the carrier | ≤ 3 s once before the relay fallback |
| `CHAT_ROTATION_QUIET = 5s` | P | Rotation waits for a lull, not just a turn boundary | Chat moves over ≥ 5 s after a turn ends |
| `PROBE_BACKOFF = 1, 2, 4, 8, 15 min` (then 15), `CARRIER_MIN_LIFETIME = 60s` | P | Bounds load on C and A on networks where direct fails; a carrier that flaps counts as a failure | Up to 15 min on relay after a transient failure on a network that works |
| `NETWORK_SETTLE = 1s`, `NETWORK_KEY_LEN = 8`, `LOCAL_NETWORK_RETRY = 10s` | P | Probes start after the path settles (the debounce restarts on each change); the user may have just tapped Allow | Upgrade starts ≥ 1 s after a network change |
| `DIRECT_OFFERS_PER_SOURCE_PER_MINUTE = 6`, `DIRECT_OFFERS_PER_NODE_PER_MINUTE = 30`, `MAX_INFLIGHT_PUNCHES_PER_NODE = 2`, `MAX_PENDING_PUNCHES = 4096`, `MAX_PENDING_PUNCHES_PER_CLIENT = 64`, `MAX_DIRECT_CONTROLS_PER_CLIENT = 32` | C; `MAX_INFLIGHT_PUNCHES_PER_NODE` is in protocol, shared with A; every per-client budget is keyed by `AddressPolicy::client_key` (an IPv4 address or an IPv6 /48) | Bounds forwarding work per client, per gateway and globally; no client fills the punch table alone | A user flapping networks more than 6 times a minute waits for `Retry-After`; more than 32 direct-capable gateways behind one address get no offers past the 32nd |
| `FIRST_TRANSPORT_MESSAGE_DEADLINE = 30s` | A (`crates/gateway/src/channel/state.rs`) | Above A's 20 s keepalive `Ping`, which P answers at the latest | A relay chat leg whose first transport message C holds back longer is closed uninstalled |

## Connection policy on P

**Relay first.**

- `RelayDialer::establish` (`app/ios/ffi/src/relay/chat.rs:47`) never consults a carrier. A chat dial always goes to the relay.
- (PR2) `dial_tunnel_leg` (`app/ios/ffi/src/relay/tunnel.rs`) dials on the live direct carrier when it is usable (not suspended, not cooling off), and on the relay otherwise. If a carrier dial fails before Noise completes, or exceeds `DIRECT_LEG_DIAL_TIMEOUT`, P **immediately re-dials that leg on the relay**, then judges the carrier:
  - **QUIC.** P retires the carrier only on connection-level evidence: `Connection::close_reason()` is `Some`, or the connection received no datagram while the dial ran (`Connection::stats().udp_rx`). Any other failure belongs to the stream (A's caps, a slow device lookup): new legs go to the relay for `CARRIER_DIAL_COOLOFF`, and the carrier and a chat leg on it stay up.

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
- An unsolicited death schedules a probe at the backoff deadline, or immediately for a carrier that lived at least `CARRIER_MIN_LIFETIME`. Recovery does not depend on another chat reconnect or path update. The end of a stream-level dial cool-off wakes the chat supervisor to reconsider rotation.
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

A's `LegDedup::install` (`crates/gateway/src/channel/state.rs:249`) aborts the displaced relay leg as soon as A has read P's handshake confirmation, which P sends the moment its own side of Noise completes, before it has installed the new pump. While a rotation is in flight:

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
- **Background.** The `.background` barrier (`app/ios/App/BayboApp.swift:41-44`) already invalidates the leg pool. (PR2) It also bumps the carrier epoch, which aborts any probe and any uncommitted rotation, and **suspends** the carrier so that no new leg dials it. The QUIC connection and its open streams, including a rotated chat leg, are left alone, as a relay chat leg is. On `.active`, P re-proves a suspended carrier: `close_reason()` must be `None`, and a fresh proof leg must complete within `DIRECT_LEG_DIAL_TIMEOUT`; that leg is parked in the pool. Otherwise P retires the carrier, and a dead chat leg goes through `leg_death`, as a relay corpse found on foreground does ([`connection.md`](../../../app/ios/docs/connection.md)).
- **Evicting a carrier.** P closes that one connection, matched by `stable_id`. It never closes the shared endpoint, which other attempts on the same socket may be using.
- **Pairing changes.** `finish_pair` and `forget_pairing` (`app/ios/ffi/src/relay/pairing.rs`) clear the carrier state, epoch and failure cache infallibly.

**No route hints.** P persists no carrier state, and `forget_pairing` clears only in-memory carrier state. Every probe must deliver P's candidates to A, because A's allowed-IP set and firewall punches depend on them, and the probe runs off the critical path, so a remembered route would save nothing.

Both foreground and background update carrier state synchronously at the Swift scene edge; only the network work is spawned. A foreground re-proof carries the carrier id and epoch. Its verdict is accepted only while that same carrier remains suspended in that epoch and the app is active. A late success cannot undo a newer background barrier, and a late failure cannot retire a replacement or a carrier another proof already resumed. Proof legs retain the pool epoch captured before the re-proof await (or at the probe's accepted upgrade); background invalidation and suspension share the carrier lock, so an old proof cannot re-enter the pool with a new epoch. Stale probe results never replace the Settings report.

## Gateway (A)

**Lifecycle.**

- **Binding scope (PR1).** `relay_content::run` (`crates/gateway/src/channel/relay_content.rs:285`) resolves `approved_relay_settings` and enters a **binding scope** (`BindingScope`) once per distinct `Ready(settings)`; the scope is left on Reconfigure, TearDown or shutdown. The scope owns the carrier runtime, started before its first control connection. Inside it, `run_binding` is the control redial loop: each iteration runs one control connection (`run_once`) and, after the backoff, re-resolves the settings, so a control redial does not restart the runtime and a control flap keeps carrier sessions up.
- **Settings identity (PR1).** `RelaySettings` (`relay_content.rs:162`) holds the approved `DeviceRow`'s `device_id`, `device_pubkey`, `auth_token_sha256` and `approved_at` beside `relay_url` and `remote_api_key`. Its equality covers the device identity and credentials, so a re-pair or credential change is a Reconfigure, just like a relay URL change.
- **Task tree.** Every QUIC stream task, before and after authentication, runs in its connection's `JoinSet`. Every connection task runs in its endpoint's `JoinSet`, under the UDP family's task in the runtime's `JoinSet`, where the punches and the registrations also run. Each of them ends when the runtime's cancellation token fires.
- **Stopping.** A Reconfigure, a TearDown (revoke) or a shutdown stops the runtime:
  1. It fires the runtime's cancellation token and waits for every task to end. Each connection shuts its stream `JoinSet` down, which drops in-flight router futures the way `legs.shutdown()` hard-aborts relay legs (`relay_content.rs:653`), and then closes itself with `CARRIER_REVOKED`. The API tunnel polls a forwarded request's router inside its session, never as a task of its own, so dropping the session drops the handler too, including one still reading an upload's body. A request from a revoked device does not run to completion, and a handler never reads a cut-off body as a complete one.
  2. It then closes the endpoints with `endpoint.close(CARRIER_REVOKED)` (QUIC application error code 1) and waits, bounded by `CARRIER_DRAIN_GRACE`, until the connections have drained (`wait_idle`) and quinn holds no reference to either socket. The runtime's own reference is then the last one, so every socket is closed when `stop` returns, and the next runtime can bind a fixed-port family again. A socket still held at the grace closes when its last connection's drain ends.
- Revocation is noticed on the 5 s `DEVICE_POLL_INTERVAL` (`relay_content.rs:61`). That is the same bound as the relay control connection. Between control connections it is noticed when the next redial re-resolves the row; a store read that fails there keeps the scope and redials with its settings.
- The runtime is started, and its UDP sockets bound, **before** control connects, so the capability in the first hello already names the families the runtime serves. Starting it derives the binding's candidate keys. When the gateway's static key cannot be read from the vault, the scope is not entered yet: the manager retries on its next `DEVICE_POLL_INTERVAL` tick, as it does for the relay node id, and connects control only once the runtime has started. Every relay leg's handshake reads the same key, so control could serve nothing meanwhile, and waiting keeps the capability in the scope's first hello. A binding whose device public key is malformed or low-order gets an inactive runtime: nothing is bound, the hellos carry no capability, and every offer is declined. So does a binding with nothing configured, or whose QUIC certificate or server configuration cannot be built. A configured family whose first bind fails keeps the runtime active and is bound again every `REBIND_DELAY`.

**Offers.**

- (PR1) Each control connection gets its own `ControlChannels` (`crates/gateway/src/relay/mod.rs:131`): the queue of signals from C and a queue of `ControlReport`s, which `pump_control` writes to C as binary JSON frames. A closed report queue stops only that arm; the connection stays up.
- A `DirectOffer` is handed to the runtime through one narrow port: `CarrierRuntime::handle_offer(punch_id, offer, register) -> ControlReport`. The report is written back on the control connection that delivered the offer: each control connection has its own report queue, and A never waits to fill it. If control redials in between, or the queue is full, the report is dropped, and C answers P with `504`. `handle_offer` is synchronous and runs inside the control loop, so it never waits: the punches, the registration loop and the rendezvous lookup run as tasks in the runtime's `JoinSet`.
- A never sends a report unprompted.
- A holds at most `MAX_INFLIGHT_PUNCHES_PER_NODE` live punches, as defence in depth against a C that ignores its own limits. An offer that passes the checks supersedes the oldest (sequence step 4).

**QUIC endpoint.** A runs one long-lived `quinn::Endpoint` per family socket, serving many connections.

- **Admission.** A lets an `Incoming` proceed only when **its source IP belongs to the allowed-IP set of a live punch** and the connection caps, keyed by `source_key`, have room. The allowed set holds P's sealed host candidate IPs, the latched `Peer` srflx IP and the sources of authenticated punches, and it lives for `PUNCH_TTL`. Everything else is `ignore()`d: no response, no Retry, and no TLS work. The packets quinn answers before an `Incoming` exists never reach it (see *Sockets*).
  - An admitted `Incoming` whose address is not yet validated gets `retry()`, which costs one RTT in a background probe. The caps are checked before the Retry as well as after it, so a source already at its cap gets no Retry either.
  - A validated one is `accept()`ed.

  Matching by IP rather than address and port also admits a symmetric-NAT phone that reaches an open or full-cone gateway. The set is a gate against scanning and CPU use, not an authentication boundary.
- **Transport limits.** `crates/carrier` pins A's `TransportConfig`:
  - `max_concurrent_bidi_streams(MAX_STREAMS_PER_CONNECTION)`;
  - `max_concurrent_uni_streams(0)`;
  - `datagram_receive_buffer_size(None)`;
  - `stream_receive_window(QUIC_STREAM_RECEIVE_WINDOW)` and `receive_window(QUIC_CONNECTION_RECEIVE_WINDOW)`;
  - the keep-alive and the idle timeout;
  - `migration(false)` on the server config: A drops every packet of an admitted connection that arrives from another address, so an admitted peer cannot move A's sends to an address outside the allowed set. P re-dials on a network change instead of migrating.

  A never reads uni streams or datagrams, so it accepts none, and nothing can be buffered for them.
- **Streams.** `accept_bi` **spawns** each stream's `DirectOpen` + Noise IK authentication into the connection's `JoinSet`, bounded by a per-connection semaphore of `MAX_STREAMS_PER_CONNECTION`. Authentication never runs inline in the accept loop, so one stalled preface cannot block the next stream. A stream is authenticated when A's side of Noise IK completes: the responder handshake calls `BinarySink::authenticated` once it has matched the device, sent msg2 and read P's handshake confirmation, and the carrier's sink reports that to its connection. A stream that ends first, whatever ends it, reports a failure.
- **First-stream deadline.** `FIRST_STREAM_DEADLINE` starts at `Incoming::accept()`, so it covers the handshake. A connection with no authenticated stream by then is closed with `CARRIER_UNAUTHENTICATED` (QUIC application error code 2) and releases its permit; one whose handshake has not completed is dropped. So is a connection whose first stream to report fails. Once a stream has authenticated, a later failure leaves the connection up.
- **Hand-off (PR1).** Every QUIC stream passes the `DirectOpen` gate (`read_open`) and one routing function, `handle_authenticated_transport` in `crates/gateway/src/channel/carrier/`, over one framed `BinarySource`. Chat goes to `run_content_session` with a `LegDedup`; Api and Blob go to `run_tunnel_session`. Both are `pub(crate)` and run over the `BinarySink`/`BinarySource` seam (`crates/gateway/src/channel/device_content.rs:325-351`) that the relay leg also runs over. The seam carries `CONFIRMS_HANDSHAKE` (see *Handshake confirmation*) and the `authenticated` hook; on the relay leg the hook only lists the leg in the link table. The hook receives the device Noise authenticated and may refuse the session; it runs before the session does anything as that device, so a refused chat leg never displaces the live one through `LegDedup`.
- **Receive errors.** A socket receive error backs off (`SOCKET_RECV_BACKOFF`, inside `DemuxSocket`) without tearing down the runtime. Only a persistent error rebinds the socket: the family checks its socket's `consecutive_recv_errors` every second, and a streak of `REBIND_AFTER_RECV_ERRORS` closes the endpoint's connections with `CARRIER_REVOKED`, closes the endpoint and binds the family again at once, retrying every `REBIND_DELAY` until it succeeds or the runtime stops. A family whose first bind fails is bound again the same way, after `REBIND_DELAY`; the failed bind logs at warn, and each failed retry at debug.

**Punching.** A sends punches only to two kinds of address:

- (a) host candidates from an offer that passed the seal, freshness and replay checks;
- (b) the latched `Peer` address of a punch that A itself answered, and only when it is `Public` IPv4 and comes from the resolved rendezvous address.

A never punches a source learned from an authenticated punch. C cannot choose any other target.

**Dedup.** `ChatLegs` (`crates/gateway/src/channel/state.rs`) spans carriers, and A does not rank carriers against each other. A stamps every chat leg with a sequence when it opens it, relay or carrier, and `LegDedup::install` makes a leg live only over a leg opened before it: the live leg is aborted, and a leg opened before the live one is refused and closes. P's supervisor is the only party that decides which chat leg is current, it never has two chat dials out at once, and it always opens its current leg last. A chat leg installs once it is proven live: a carrier chat leg when P's handshake confirmation decrypts, a relay chat leg when its first transport message from P does (P's `Subscribe`, or at the latest its `Pong` to A's keepalive `Ping`), within `FIRST_TRANSPORT_MESSAGE_DEADLINE`; a relay chat leg silent past the deadline closes uninstalled. C opens every relay data leg and can replay a relay leg's msg1, and A answers it with msg2, but only the initiator that wrote that msg1 can produce a transport message after it, so a replay never installs. C can also hold back a genuine relay leg's first transport message and release it later, but by then any leg P moved its chat to was opened after it, so the release only closes the held leg.

**Link table.** `DeviceLinks` (`crates/gateway/src/channel/links.rs`, PR1) is an in-memory table per device, one per gateway process (`GatewayDeps::device_links`). It records the live legs, each with its `LegClass`, its `CarrierKind` and a start time, together with the last offer's time and outcome. Its writers are the relay path and the carrier runtime; its write verbs, `tracked` and `record_offer`, are crate-private, so nothing outside the gateway crate writes it:

- Every leg's sink is wrapped by `DeviceLinks::tracked`: a relay content or tunnel leg with `Relay`, a carrier session in `handle_authenticated_transport` with its kind. The wrapper lists the leg in the `authenticated` hook, under the device Noise authenticated and only once the wrapped sink has accepted it, and unlists it when the sink drops with the session. A leg is therefore listed from authentication until it ends, and a refused or unauthenticated one never is.
- `CarrierRuntime::handle_offer` records the outcome of each offer that opened under the binding's key as the binding device's last offer, replacing the one before. An offer that does not open (`declined:auth`) or arrives with nothing bound (`declined:unbound`) is logged but never recorded: anyone holding the node id can make C forward one. An inactive runtime sends no capability, so no offer is meant to reach it, and it records nothing.

A device leaves the table once it has neither a live leg nor a recorded offer. `GET /v1/mobile/links` (`crates/gateway/src/api/admin/mobile.rs`, which also holds its wire types) serves the table as `MobileLinks`: devices by id, each with its legs oldest first and its last offer. A leg's `class` and `carrier` are spelled as `LegClass` and `CarrierKind::as_str` spell them, and an offer's `outcome` as the `direct_offer` log line does (`accepted` or `declined:<reason>`). A QUIC carrier's kind comes from the address pair as A sees it, through `CarrierKind::of_quic_path` in `crates/carrier`. It names the A candidate P dialed, which is A's local address except behind A's IPv4 NAT, so P derives the same kind from the candidate it dialed:

- IPv6 has no NAT, so A's local address alone decides: `Lan` for a ULA and `Ipv6` for a GUA, whatever P's address is;
- `Ipv4` when A's local address is public IPv4;
- `Lan` when both ends are private IPv4;
- `Ipv4Punched` when A's local address is private and P's is public IPv4, meaning P reached A through A's NAT mapping.

The one pair on which the labels differ is a hairpinned punch: P on A's own network dials A's srflx, and A's router loops the traffic back with a private source (P's own address, or the router's when it rewrites the source). A then sees two private addresses and lists the leg as `lan`, while P, which dialed the mapping, shows `Ipv4Punched`. A has no view of the address P dialed, so it does not guess.

A's local address is the destination of the datagrams the connection receives, which the demux socket reports to quinn (`quinn::Connection::local_ip`), so a wildcard-bound socket, the default, still knows it; a socket bound to a specific address falls back to that address. A QUIC leg is listed with a `null` carrier when A's local address is unknown (a wildcard-bound socket on a platform that does not report datagram destinations) or its pair has no kind: an address the policy excludes, such as loopback outside tests, or a pair of mixed families.

### Config

`gateway.*` is not hot-reloadable; changes take effect on restart. **`crates/config/src/validate.rs` is the only home of the rules.**

- The runtime receives typed values through `RuntimeGatewayConfig::carrier` (`RuntimeCarrierConfig` in `crates/gateway/src/config.rs`, whose `udp` is `None` when `direct_udp.enabled` is false) and copies them without re-checking.
- `validate.rs` owns shape rules. `AddressPolicy` owns address classes, and the gateway applies it when it gathers candidates. Neither re-implements the other, so `baybo-config` does not depend on the protocol crate.

```jsonc
"gateway": {
  // On by default whenever a binding exists; absent ⇒ defaults.
  "direct_udp": { "enabled": true, "ipv4_bind": "0.0.0.0:0", "ipv6_bind": "[::]:0" }
}
```

The binds are typed `Option<SocketAddr>`, so an unparseable address fails deserialisation. `validate_gateway` (`crates/config/src/validate.rs:260`) adds these rules:

- **Family:** each `*_bind` must match its family, also while `direct_udp` is disabled. An `ipv6_bind` must not be IPv4-mapped (`[::ffff:a.b.c.d]`), because the IPv6 socket sets `IPV6_V6ONLY` and could not bind it.
- **At least one family:** `direct_udp.enabled` requires at least one bind.

`direct_udp.enabled: false` is the off switch, and a `null` bind leaves that family unbound. An absent `direct_udp` field takes its default, so an explicit `null` is the only way to turn one family off, and it survives a rewrite of the file.

A gateway in a container needs host networking for direct UDP. On a bridge network, the host's NAT can confirm conntrack state for P's early inbound punch before A's own first outbound, and it then remaps A's port.

## C (remote-host)

- **Routes.** C serves one HTTP route for direct carriers, `POST /direct/{relay_node_id}`, behind the same `require_admitted` route layer and outer per-IP limiter as the five WebSocket relay routes (`relay_routes`, `remote-host/crates/relay/src/serve.rs:346-373`), and one UDP socket.
  - `is_relay_route` includes `/direct/`, so refusals on this route are logged.
  - The edge traffic ledger records the route as `direct/offer`.
  - POST is used because the request carries a body, mints state, and must never be cached. A layer outside the per-IP limiter marks every response on the route `Cache-Control: no-store`, so the limiter's `429`, admission's `401` and the extractors' `400`/`413` carry it too.
- **No device authentication at C.** C does not authenticate the device on this route: A does, through the seal.
  - A holder of the tenant key and the node id can make C forward garbage. A declines it after one AEAD open, with no punch and no registration.
  - On the built-in proxy every device shares the `guest` key ([`relay-push-security.md`](relay-push-security.md#shared-relay-key-tenancy-and-push-binding-authentication)), so the owner check is not a tenant boundary there, and the node id is the only gate. The node id is not secret: it appears in `/content/join/{relay_node_id}` URLs, which a fronting CDN sees, and in the gateway's info-level log.
  - The offer budget is therefore keyed per (node, client) under a higher per-node ceiling. The client is `AddressPolicy::client_key` of the IP that the shared client-IP resolution of the per-IP limiter yields (`CLIENT_IP_HEADERS`): an IPv4 address or an IPv6 /48, so a client cannot multiply its share by rotating through the /64s of its own delegation. The per-node ceiling remains shared: clients that together hold more than `DIRECT_OFFERS_PER_NODE_PER_MINUTE / DIRECT_OFFERS_PER_SOURCE_PER_MINUTE` distinct addresses or /48s can spend one gateway's offer budget, which costs that gateway its direct upgrades until the window slides, never its relay.
- **Punch registry** (`remote-host/crates/relay/src/punch.rs`, PR1). The registry maps `PunchId → { node, owner key, control token, answer oneshot, keys per role, client, latched srflx per role, datagram count, created, answered, paired }`. `ControlRegistry` owns it. `ControlRegistry::offer_direct` checks the route, reserves a slot on the control channel and only then asks the registry for a punch, and the registry returns the POST's `PendingOffer`, which waits up to `DIRECT_ANSWER_TIMEOUT` for the report that `ControlRegistry::report_direct` routes to it. The UDP rendezvous reaches the registry through `ControlRegistry::punches`.
  - A punch counts toward `MAX_INFLIGHT_PUNCHES_PER_NODE` from creation until one of these: its POST ends with a non-200 status, or with a 200 that carries no rendezvous (both drop the entry at once, including a POST whose client goes away); or C has sent `Peer` to both roles. After that the entry only answers re-sent `Register`s until it expires.
  - The offer budgets are sliding one-minute windows of admitted offers. Every `429` carries `Retry-After`: for the in-flight cap, the time until the node's oldest in-flight punch expires; for a rate, the time until its window has room; when several limits refuse, the longest of these waits. The `503` for `MAX_PENDING_PUNCHES` carries the time until the oldest punch expires. `Retry-After` is in whole seconds, rounded up, at least 1.
  - Entries expire at `PUNCH_TTL` at the latest; a paired punch `PEER_WAIT` after pairing, the longest either role keeps re-sending its `Register`; and an answered punch P has not registered for `REGISTRATION_GRACE` after its `200`, since it can never pair. Every lookup ignores an expired punch, and a sweep drops it, run on each new offer and every `PUNCH_SWEEP_INTERVAL` by the rendezvous loop. A control connection's punches drop when it unregisters or a reconnect supersedes it, and a POST still waiting on one then ends with `504`.
  - The registry enforces `MAX_INFLIGHT_PUNCHES_PER_NODE`, `MAX_PENDING_PUNCHES_PER_CLIENT` (a `429` with `client_pending` and the time until the client's soonest punch expires) and `MAX_PENDING_PUNCHES`.
  - C honours the direct capability of at most `MAX_DIRECT_CONTROLS_PER_CLIENT` control connections per client (`ControlRegistry::register_from`). A connection past it still registers and relays, but C treats it as not direct-capable, so one client cannot spread punches over unboundedly many nodes. Control connections are exempt from the per-key connection cap, and on the built-in proxy every gateway shares the public `guest` key, so the client is the only boundary.
  - C keeps **no per-gateway UDP state outside a punch**: no heartbeat and no standing registration.
  - (PR1) `MAX_RELAY_NODE_ID_BYTES` is enforced wherever a node id becomes a map key, through one predicate (`control::node_id_within_bound`). `ControlRegistry::register` refuses a longer id, `offer_direct` answers it with the opaque `404`, and `/content/join/{relay_node_id}` answers it `503 gateway not connected` before the id reaches the pending content-leg table or the refusal-log debounce.
- **Control reports.** (PR1) After the hello, the inbound branch of `run_control` (`serve.rs:879-892`) parses every binary frame as a `ControlReport` (`handle_control_report`). A report is honoured only for a punch owned by *this* node's *current* control token, and only once. A malformed or foreign report, a repeated one, and a `DirectAnswer` whose answer exceeds `MAX_SEALED_CANDIDATES_BYTES` are logged at debug level (a malformed frame by its length and parse position only, never its bytes) and ignored, and **never close control**. Every inbound frame, a report or not, counts as liveness against the control idle timeout, and only inbound frames do: C's own signals, offers included, never extend it, so a half-open gateway is closed at the idle timeout even while phones keep posting offers for it.
- **UDP socket** (`remote-host/crates/relay/src/udp.rs`, PR1).
  - **Knobs.** `.env` has `UDP_PUBLIC_ADDR` (blank = off), `UDP_PORT` (default 7777, the same on host and container) and an optional `UDP_BIND_ADDR`, for host networking only. The compose file publishes the port as `0.0.0.0:${UDP_PORT}:${UDP_PORT}/udp` and sets `UDP_BIND_ADDR: ${UDP_BIND_ADDR:-0.0.0.0:${UDP_PORT:-7777}}`, mirroring `PORT` → `BIND_ADDR`. Without the variable, the process binds `DEFAULT_UDP_BIND_ADDR`.
  - The socket starts only when `UDP_PUBLIC_ADDR` is set, and is bound before any offer can name it. `UDP_BIND_ADDR` must be an IPv4 socket address; any other value, or a bind that fails, fails startup.
  - `UDP_PUBLIC_ADDR` is normalised at startup: it is a `Public` IPv4 literal or an LDH hostname, plus a port. A DNS name is not resolved at C, and an invalid value fails startup. Clients resolve it with `resolve_public_v4`.
  - The rendezvous is IPv4 by design, because its job is to observe IPv4 NAT mappings. IPv6 connectivity needs no C. `::ffff:` sources are canonicalised. A `Register` from a source that is not IPv4 is dropped (`udp_source_not_ipv4`), and so is one from a source that is not `Public` or has port 0 (`udp_source_not_public`). Both warnings are rate-limited to one per `UDP_SOURCE_WARN_INTERVAL`, which makes a source-rewriting deployment visible.
  - A `Register` is answered only for a live punch, only when its tag verifies under that role's key, and only from the source latched for that role (sequence step 9). Only such a `Register` counts toward `MAX_DATAGRAMS_PER_PUNCH`, so a forged one or a copy replayed from another source spends nothing. Anything else, including a datagram that is not a `Register` or is longer than one, is dropped silently, so C offers no oracle.
  - Every `Register` naming a live punch, valid or not, counts toward `MAX_DATAGRAMS_PER_PUNCH`; later ones are dropped.
  - Receive errors back off (`SOCKET_RECV_BACKOFF`) without stopping HTTP or WSS.
- **What C logs.**
  - Per offer: node, `key_tag`, a `key_tag` of the punch id, status, elapsed time. An offer refused with no direct route logs at debug level, with its reason.
  - Per punch with a rendezvous, when it ends (expired, control closed, or its POST settled): whether each role registered, and the time from the offer to the first `Peer`.
  - **Never** keys, sealed blobs, or the observed addresses at info level. Addresses appear at debug level only.

## Compatibility and rollout

| A | C | P | Outcome |
|---|---|---|---|
| any | any | old | P never posts. Nothing changes. |
| old | new | new | No capability in the hello ⇒ `404`. P stays on the relay and backs off. |
| new | old | new | The route does not exist ⇒ `404`, same as above. The old C ignores the extra hello field (serde ignores unknown fields). |
| new | old | old | A binds its sockets and advertises; nothing ever arrives. |
| new, `direct_udp.enabled: false` | new | new | No capability ⇒ `404`. |
| new | new, no `UDP_PUBLIC_ADDR` | new | Offers flow without `register`: LAN, IPv6, public-IPv4 host candidates (admitted by authenticated punches, which a stateful host firewall on A drops unless it accepts inbound UDP on a fixed `direct_udp` port; see *Sockets*) work; `Ipv4Punched` does not. |
| new | new | new | Full design. |

The built-in public proxy (`proxy.baybo.space`) is to run with the UDP rendezvous enabled once C is deployed after PR1 merges (planned). A self-hosted C opts in by setting `UDP_PUBLIC_ADDR`.

**Deploy order:** C, then gateways, then the app. Any order is safe, but only C-first gives the app something to use on its first release. `DirectCapability.version` and `CANDIDATE_SET_VERSION` carry future changes: an unknown version reads as "not capable", never as an error.

## Security

This section records the delta against [`relay-push-security.md`](relay-push-security.md#security-boundaries).

**New in-scope parties:**

- an on-path LAN attacker between P and A;
- network observers on the direct path;
- an on-path observer between a side and C's UDP rendezvous, who can read and forge UDP there;
- C forging, replaying or withholding candidate advertisements and `Peer` messages.

**C can, in addition:**

- **Replay a relay leg's handshake.** C opens every relay data leg and sees its Noise msg1, which carries no replay protection. A answers a replayed one with msg2, lists a relay leg for the device in the link table and bumps its `last_seen`, until C closes the leg or its control connection ends; the session never decrypts anything, and it never displaces the device's live chat leg (see *Dedup*).
- **Observe new metadata.** C sees P's and A's IPv4 UDP mappings (ip:port), their NAT behaviour as it is visible from one vantage point, and the time, frequency and C-side outcome of every direct attempt.
- **Force the relay.** C can drop, delay or answer falsely to `POST /direct`, `DirectOffer`, `DirectAnswer`, `Register` or `Peer`. The effect is availability only: P stays on the relay.
- **Steer registrations, punches and one admission.** For each offer that P really sealed, C chooses:
  - one `Public` IPv4 rendezvous address, via a lookup of a name C picked, of which only the IPv4 (A-record) results are used. A and P each send it at most `PEER_WAIT / REGISTER_RETRY_INTERVAL` datagrams of `REGISTER_DATAGRAM_LEN` bytes;
  - one latched `Public` IPv4 srflx address per side. It receives `PUNCH_BURST` punches from the other side and, from P, QUIC Initials for up to `PROBE_BUDGET`;
  - one `Public` IP in A's allowed set for `PUNCH_TTL`: the srflx it names for P.

  That admission lets C reach A's pre-authentication QUIC surface and fetch A's per-process certificate. It still ends at the token gate and Noise. An offer replayed across a gateway process restart while P's clock runs ahead of A's (see *C cannot*) counts as one more such offer, and like any accepted offer it may supersede one of the device's live punches.

**C cannot, assuming endpoint keys stay secret:**

- **Read host candidates from the sealed sets.** The sets hide P's and A's LAN addresses, ULAs, GUAs and public IPv4 interface addresses, and the direct token. C learns a host candidate only when it is also that side's HTTPS/WSS source address, which it sees anyway: for example the GUA P posts from, or the address of a gateway without NAT. It cannot learn how many candidates either side has.
- **Tamper with sets.** C cannot inject, alter or drop an individual host candidate. Any tampering fails the AEAD, and the whole set is rejected.
- **Point either side at a private address, or at itself.** C cannot make A or P send to a `Lan` or excluded address, or to one of the side's own interface addresses. Both sides require the rendezvous and `Peer` addresses to be `Public` IPv4 and none of their own (`carrier::rendezvous::OwnAddresses`, built from the interface enumeration A runs for each offer anyway), and private targets come only from sealed, authenticated sets. A side behind a NAT does not know its own public address, so C can still name it: a router that hairpins then delivers the datagrams to a port it forwards. Those are `REGISTER_DATAGRAM_LEN`-byte `Register`s whose first byte is `PROBE_DATAGRAM_MAGIC`, or `PUNCH_BURST` inert punches, per genuine offer.
- **Admit any other source at A.** An authenticated punch needs `k_punch`, so C cannot forge one. An observer on the P→A path can race a copy from its own address, which admits that address to the same pre-authentication surface, at most `MAX_PRFLX_SOURCES_PER_PUNCH` per punch.
- **Replay an old answer to P.** The `offer_id` echo prevents it.
- **Displace a live chat leg by holding back a relay leg.** A held-back relay chat leg was opened before any leg P moved its chat to, so its late first message closes it instead (see *Dedup*).

**An on-path observer of the UDP rendezvous** sees punch ids and both sides' mappings, but no key. It cannot forge a `Peer` or `Registered` to either side, or a `Register` to C, and it cannot spend a punch's datagram budget. It can still race a copy of a side's `Register` from its own address before the real one arrives, which makes C latch that address as the side's mapping for the punch, as it can race an authenticated punch toward A. The cost is that punch's punched-IPv4 tier.
- **Get a replayed offer accepted**, with one exception. The replay cache lives for the gateway process, so it declines a repeat across Reconfigures, and a new process declines any offer issued before it started by P's clock. The exception is a restart while P's clock runs ahead of A's: an offer C captured within that lead (at most `OFFER_MAX_AGE`) before the gateway process restarted is accepted once by the new process, which then treats it as one more genuine offer (see *C can*).
- **Mint an offer that A accepts.** Doing so needs P's static secret.
- **Read, modify or misroute content on a direct carrier**, or impersonate either end. This is Claim 5.

**Claim 5: a carrier session is exactly as trustworthy as a relay leg.** A carrier session delivers no application byte before Noise IK completes between the pairing statics and P confirms it. A checks the initiator against its approved device rows (`lookup_approved_by_pubkey` in `responder_handshake`, `crates/gateway/src/channel/device_content.rs:261`), and P checks the pinned gateway static. The `DirectOpen` token, QUIC's TLS and the allowed-IP set are availability gates only. Claim 2 therefore holds verbatim with "carrier" in place of "relay leg". The sealed candidate sets and the `Public`-IPv4 checks add the guarantee that C cannot steer either side's direct traffic toward any private address. Toward public addresses, C can steer only what *C can* lists above, per genuine offer.

**DoS and amplification.**

- **C's UDP** replies at most once per `Register`, only to its sender, never with more bytes than it received, and only for live punches with a `Register` that verifies under its role key. It cannot reflect or amplify.
- **`POST /direct`** is bounded by the per-IP bucket, the per-(node, client) rate, the per-node ceiling, the in-flight cap and the per-client punch cap; the punches of one client never exceed `MAX_PENDING_PUNCHES_PER_CLIENT`.
- **A** does no handshake work for sources outside its allowed set, sends a Retry to unvalidated ones, and pins its QUIC transport limits so that an admitted, unauthenticated peer cannot make it buffer data it never reads. QUIC's 3× anti-amplification limit applies to the admitted sources.
- **P** sends at most `MAX_PUNCH_DATAGRAMS_PER_PROBE` punches and one QUIC attempt per remote candidate per probe, only toward sealed candidates it may dial and the one latched srflx.

**LAN probing.**

- A's private targets are exactly the addresses the legitimate P sealed about itself. P omits cellular private addresses.
- A may still punch a P LAN address that coincides with an unrelated host on A's own LAN, for example when P is on a different Wi-Fi that uses the same subnet. That costs `PUNCH_BURST` small, inert datagrams per address per genuine probe.
- P dials A's `Lan` candidates only from a Wi-Fi or wired interface that has a `Lan` address of that family: at most `PUNCH_BURST` punches and one QUIC attempt per candidate per probe.

**Privacy delta.**

- A and P now learn each other's addresses. On the relay, each saw only C.
- Network observers on either path see that P talks to A.
- C sees less traffic metadata while a carrier is live, because that traffic bypasses it.

Browser remote access is out of scope and is not affected.

## Observability

- **Carrier model.** `CarrierKind { Relay, Lan, Ipv6, Ipv4, Ipv4Punched }` is shared by A's link table and P's state. A probe has one tier per direct kind (`lan`, `ipv6`, `ipv4`, `ipv4_punched`), and records an outcome for each:
  - `ok`, `failed` or `timeout`;
  - `not_offered`: no candidates of that tier, no rendezvous, or a path that may not dial it;
  - `denied`: the iOS Local Network permission is refused or pending (see *The prober*);
  - `skipped`.
- **iOS.** `SettingsScreen` shows one tappable **Connection** row with "Via relay server", "Local network", "Direct · IPv6", "Direct · IPv4" or "IPv4 traversal". `ConnectionDetailsScreen` shows the last connection check time, network and per-path outcomes, explains the active route and router traversal, and offers a live connection console with copy/clear and tail-following. Capture is bounded in memory and stops on leaving the details page; dedicated events exclude secrets and conversation data (see [`connection.md`](../../../app/ios/docs/connection.md)). State reaches Swift through `CarrierSink`. **The chat screen gets no badge**: the chat header keeps showing only `legDown`.
- **Gateway.** `baybo device status` (PR1) lists, for each approved device, its approval and last-seen times, its live legs by class with their carrier and start time, and its last offer's outcome and time.
  - It reads the running gateway's admin route `GET /v1/mobile/links`, authenticated with the vault's admin token, at the address `baybo_gateway::config::admin_dial_addr` derives from `gateway.bind_address` and `gateway.port` (a wildcard bind is dialed on loopback, as `baybo tui` does). It is the first `baybo device` command that queries the running gateway instead of the stores. When no link table comes back, it says why in one line (nothing answered, the gateway refused the token, or the vault holds none) and prints the device rows alone. With `--json`, `gateway.error` carries that line and each device's `legs` is `null`, not empty, while unknown.
  - It is shell-only, like the rest of the `device` family, which `crates/cli/src/slash.rs` already rejects as a whole.
- **Structured logs,** as key=value fields. Addresses appear only at debug level.
  - **P:** `direct_probe probe trigger network=<tag> outcome elapsed_ms tiers="lan=… ipv6=… ipv4=… ipv4_punched=…"`, `direct_attempt tier family candidate_class outcome elapsed_ms`, `peer_conflict punch`, `carrier_up kind`, `carrier_down kind reason lifetime_ms`, `carrier_cooloff reason`, `chat_rotation outcome waited_ms`.
  - **A:** `direct_offer punch=<tag> device outcome=accepted|declined:<auth|stale|replayed|over_cap|unbound>` (plus `superseded=<tag>` when it replaced a punch; `unbound` when no socket is bound, which includes a binding without candidate keys), `udp_register punch outcome=peer|no_peer|no_reply|unresolved`, `peer_conflict punch`, `punch punch targets datagrams`, `punch_admit punch sources`, `quic_incoming` (counters of `accepted`, `retried`, `ignored_not_in_punch` and `ignored_cap`, counted per runtime and logged at most once per `INCOMING_LOG_INTERVAL` across its families and rebinds), `carrier_session class kind device`.
  - **C:** as listed under *C (remote-host)*.

## Failure modes

| # | Scenario | Detection | Behaviour | User cost |
|---|---|---|---|---|
| 1 | C, A or P lacks support | `404` | Stay on the relay; back off | none |
| 2 | A's side is symmetric NAT, P is symmetric and A port-restricted, or UDP is blocked | No QUIC handshake within `PROBE_BUDGET` | Stay on the relay; back off per network | none |
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
| 14 | IPv6-only path without CLAT | No IPv4 route | IPv4 tiers `not_offered`; IPv6 still runs | Relay if A has no GUA |
| 15 | Device revoked | 5 s poll → TearDown | Runtime stops; carrier sessions and their in-flight requests dropped; relay Noise lookup fails too | By design |

## Testing

**Unit tests (PR1):**

- **protocol:**
  - serde round trips; an old hello decoding with `direct: None`; lenient decode of a malformed capability;
  - the `AddressPolicy` class table, `source_key`, and the production policy rejecting a loopback or `Lan` rendezvous address in `resolve_public_v4`;
  - the `ProbeDatagram` codec, including a property test that every encoded datagram has `0x80|0x40` clear and that a `Registered` or `Peer` is never longer than a `Register`; a rendezvous tag verifies only under its own role key and with every field it covers;
  - `client_key` keys an IPv4 address or an IPv6 /48;
- **device-proto:**
  - seal/open round trips; directional separation (a set sealed under the other direction's key does not open, even at the right length and associated data; a set reflected to its sender is refused by its length); AAD binding (a wrong node id or punch id fails);
  - low-order peer keys (u = 0, u = 1 and an order-8 point) are rejected; tampering with the ciphertext or the nonce fails; every seal draws a fresh nonce;
  - an empty set and a full-cap all-IPv6 set seal to the same length, as do an empty answer and a full-cap one; the pinned plaintext lengths hold the widest full-cap bodies, whose body and sealed wire lengths are pinned exactly; fixed-size byte fields encode at a fixed length;
  - an oversized or over-cap body fails to seal; a wrong-length plaintext, non-zero padding, an overrunning declared length, bytes after the body inside the declared length, a malformed body, a version mismatch and an over-cap count fail to open; an oversized, non-base64 or short-nonce set is refused before the AEAD;
  - an answer that does not echo the offer id is refused; punch tags verify and reject; `CertHash` pins one certificate.
- **carrier:**
  - demux routing: a synthetic GRO buffer holding `Peer ‖ Registered` yields both datagrams, QUIC segments around a probe datagram are packed on their stride, a malformed probe datagram reaches neither the queue nor quinn, a long-header segment with another version, a short Initial destination connection ID or a truncated header never reaches quinn, and an IPv4-mapped source reads as IPv4; over a real socket, a probe datagram reaches the queue and `send_probe` leaves from the same socket;
  - a carrier server sends nothing in reply to an unknown-version datagram or a short-destination-ID Initial, both of which a plain quinn server answers before any `Incoming`;
  - a probe send error reaches the caller while a QUIC send absorbs it; a receive error of any kind is absorbed rather than handed to quinn, only a real one starts a backoff, and the next receive clears the streak; an IPv6 socket is IPv6-only; a socket carries one endpoint;
  - burst pacing (`start_paused`): rounds at `PUNCH_INTERVAL`, no catch-up volley, a cancelled wait releases nothing;
  - framing: round trips, a clean end, the size cap on write and on read, a reader poisoned after an error, truncation, a cancelled read that keeps its bytes, a reader whose `Debug` never prints the frame it is assembling, and a malformed `DirectOpen` refused without echoing the token (the secret sits where serde's own message would quote it);
  - the `grease_quic_bit` transport parameter is absent in both directions (and quinn's default configuration would send it);
  - a pinned handshake negotiates `DIRECT_QUIC_ALPN` and carries a framed stream; a server presenting a different self-signed certificate fails the handshake before any stream opens; the verifier accepts only the pinned certificate presented alone;
  - A grants `MAX_STREAMS_PER_CONNECTION` bidirectional streams and refuses uni streams and datagrams; P grants A no stream and no datagrams.
- **relay (C):**
  - `POST /direct` returns `401`/`404`/`429`/`503`/`504`/`200` with owner and capability gating, and `400`/`413` for a malformed or oversized body; every `404` is the same, and no response is cacheable, including the per-IP limiter's `429` and admission's `401`; a POST waiting on a control connection that closes ends with `504`;
  - the per-(node, client) budget keys on the header-resolved client IP, an IPv6 /48 counting as one client;
  - the per-source and per-node windows slide; a refused offer consumes no budget, including one refused for a full or closed control channel; the longest wait wins; `MAX_PENDING_PUNCHES`; the in-flight release rules; the punch TTL (an expired punch answers nothing before the sweep drops it); a control disconnect or a reconnect drops that connection's punches;
  - a report settles only its own connection's punch, once, and an oversized answer is not forwarded; a malformed capability or report keeps control; a gateway that sends nothing is closed at the control idle timeout while C keeps signalling it; a node id over `MAX_RELAY_NODE_ID_BYTES` never becomes a key, at registration, on the offer route or on a content join;
  - role keys are not interchangeable and C's replies are tagged under the role's key; unknown punch ids, malformed or wrongly tagged datagrams and punches without a rendezvous get no reply; a `Register` for an observed role from a new source is dropped; non-IPv4 and non-`Public` sources are ignored and an `::ffff:` source is canonicalised; each role gets the other's mapping over real sockets; only verified `Register`s from the latched source spend `MAX_DATAGRAMS_PER_PUNCH`;
  - an answered punch P never registers for expires after `REGISTRATION_GRACE`, and a paired one `PEER_WAIT` after pairing; one client holds at most `MAX_PENDING_PUNCHES_PER_CLIENT` punches; past `MAX_DIRECT_CONTROLS_PER_CLIENT` a client's control connection is not offered punches; every IPv6 /64 of one /48 shares one offer budget;
  - per punch, C emits no more datagrams or bytes than it receives;
  - receive errors back off and a received datagram resets the streak; the unusable-source warnings are rate-limited; `UDP_PUBLIC_ADDR` and `UDP_BIND_ADDR` are validated.

  The no-answer path uses `ControlRegistry::with_direct_answer_timeout` (relay `test-support`) instead of sleeping for `DIRECT_ANSWER_TIMEOUT`; the UDP loop's tests drive a scripted socket, so they can inject any source and receive error.
- **gateway:**
  - offers are declined on auth, stale (including an offer issued before the process's carriers started), replay (including on the replay window's last millisecond, and in the next runtime of the same process, for an offer P's fast clock issued after that runtime started) and a full replay cache; a new offer supersedes the oldest punch at the cap;
  - `Peer` is latched (a conflicting one is logged and dropped), and a lost first `Peer` is recovered by the next `Register`;
  - a valid authenticated punch admits its source and a bad tag does not; an `Incoming` outside the allowed set is ignored; an unvalidated one gets a Retry, unless its source is at its cap;
  - a stalled preface on stream 1 does not block stream 2; the first-stream deadline holds, and a handshake that never completes releases its permit at `FIRST_STREAM_DEADLINE`;
  - stopping a binding closes its active sessions even mid-request (`stopping_a_binding_closes_its_active_quic_sessions_even_mid_request`): an Api upload parked in its handler has its future dropped, never seeing its body end, by the time `stop` returns;
  - interfaces are not enumerated when nothing is bound, and once per offer otherwise; an excluded or port-less device candidate is dropped and never admitted; a declined offer is never punched and never registers; a private `Peer` is ignored and registration goes on; a binding without candidate keys stays inactive;
  - a UDP family whose first bind fails (its fixed port still held) is named in the first hello, declines offers as `unbound`, and is bound again after the rebind delay, then serves;
  - a persistent receive error rebinds the family, whose connections close with `CARRIER_REVOKED`, and the new socket serves; the test feeds the real socket's receive-error streak through `DemuxSocket::inject_recv_errors` (carrier `test-support`), so the family's own health poll decides;
  - a QUIC Api session runs the tunnel; a replayed `DirectOpen`, msg1 and confirmation on a carrier stream never authenticate and never displace the live chat leg, while a confirmed carrier chat leg does displace it;
  - `LegDedup` works across carriers, and a relay chat leg installs only at its first decrypted transport message, so a relay msg1 C replays never displaces a live carrier chat leg; a leg opened earlier never displaces one opened later, so a relay chat leg whose first message C held back closes instead of displacing a later carrier chat leg; a relay chat leg silent past its deadline closes uninstalled; an offer that does not open leaves the device's last offer alone; an admitted QUIC connection that moves to a new address is not followed; the `CarrierKind` classification, including a mixed ULA/GUA IPv6 pair in both directions, which takes the class of A's address; the config validation table.
  - the link table: a leg is listed, under the device Noise authenticated, from its `authenticated` hook until its sink drops, and never when the wrapped sink refuses the device; a QUIC chat leg is listed with its path's kind, on a socket bound to a specific address and on a wildcard-bound one, whose kind comes from the datagrams' destination, over real loopback carriers, and a relay chat leg as `relay` in the relay E2E; each offer's outcome replaces the last and outlives the legs; the wire labels equal `CarrierKind::as_str`, `LegClass` and the `direct_offer` outcomes; `GET /v1/mobile/links` refuses a missing or wrong bearer and serves what the writers recorded; `baybo device status` against a stand-in gateway joins the table to the approved rows, and prints the rows alone, saying why, when nothing answers, the gateway refuses the token, or the vault holds none.
  - the binding scope: a control flap stays inside it and never restarts the runtime; a static key the vault cannot read yet delays control until the runtime starts, so the first hello carries the capability; the runtime holds its sockets from the first hello until a revoke ends the scope, and releases them after; a stopped runtime's sockets are closed when `stop` returns, also with a connection draining, so the next runtime binds the same fixed port; a hello carries no capability when the runtime is inactive; a report goes back only on the connection that delivered its offer; stopping drops every task before any endpoint closes; a re-pair or credential change is a Reconfigure.

**E2E (PR1)** lives in `crates/gateway/src/channel/relay_e2e.rs`, which boots a real in-process C; for the direct cases C also runs its UDP rendezvous on loopback. The gateway side is the real relay-content manager (`relay_content::run` with millisecond polls), so the binding scope, its carrier runtime, the hello's capability and the report path run as shipped. The crate builds with `test-support`, so `AddressPolicy::for_tests()` treats 127/8 as `Public` and `::1` as `Lan`. A mock device, driving the phone's end of a carrier that `crates/gateway/src/channel/carrier/phone.rs` (test-only) shares with the runtime's tests, seals an offer, posts it, gets A to admit it the ways P's tiers do (a `Peer` from C's rendezvous, an authenticated punch alone, or a host candidate in its offer) and connects over QUIC:

- the rendezvous path of P's `ipv4_punched` tier runs over 127.0.0.1. The phone registers, learns A's mapping from `Peer`, waits for A's punch and dials the mapping. Its offer carries no host candidate, and since loopback has no NAT to open it sends no punch of its own, so C's `Peer` is the only thing that can admit it: A allows the mapping it latches before it punches it. The mapping is A's host address, which the test policy classes `Public`, so A lists the leg as `ipv4`; A labels a leg `ipv4_punched` only behind a NAT, as in the netns matrix;
- the `Lan` carrier runs over `[::1]`, where A, serving no IPv4 UDP family, gets no rendezvous. The offer carries no host candidate either, so the phone's authenticated punch alone admits it; an Initial A judges before the punch is ignored, and quinn's retransmit gets in. On a host that cannot bind `::1` the case skips itself, as the runtime's IPv6 test does;
- each completes a chat frame round trip;
- revoking mid-session closes the carrier: its QUIC connection, admitted by the host candidate the phone offers, closes with `CARRIER_REVOKED`, ending its chat and API sessions. The legs leave the link table, and C, whose control connection has closed, answers the next offer `404`;
- a tampered offer is declined by A, C answers it with its opaque `404`, and the device's last offer in the link table stays unset.

**Unit tests (PR2):**

- the prober state machine: single flight, backoff steps, a new network key probing immediately, stale-epoch discard, `429` leaving the cache alone, an unrepresentable `Retry-After` falling back to the longest backoff, `denied` waiting once on its own network before re-probing, a P-initiated retirement leaving the cache alone, a cellular path not dialing `Lan` candidates;
- `network_changed`: two identical paths cause no retirement; cellular toggling under Wi-Fi causes none; Wi-Fi to cellular causes exactly one; a new subnet on the same interface invalidates the old probe;
- rotation: the idle predicate, the commit point, sends held during a rotation, `PumpEnded` held during a rotation, each abandonment cause;
- a stream-level failure keeping the carrier; early and late unsolicited deaths returning the next probe deadline; the background suspend and foreground re-proof, including a second background/foreground cycle, a network change, and a duplicate proof finishing after a successful one;
- pool invalidation on every transition; `forget_pairing` clearing carrier state.

**The netns NAT matrix (PR2)** is `#[ignore]`d because it needs root. A non-gating `netns-matrix` CI job (`continue-on-error`, PR-only, path-filtered on the carrier code of all three sides) runs `scripts/netns-matrix.sh`. The script builds the three workspaces' binaries with `test-support` (`remote-host-protocol/test-support` for C, `baybo-gateway/test-support` for A, which forwards it, and the ffi's own `test-support` for P), passes their paths to the test through `NETNS_*_BIN`, and runs `sudo -E cargo test … -- --ignored`. `NETNS_CELLS`, `NETNS_RUNS` and `NETNS_IDLE_SECS` narrow and size a local run. It drives three real processes, each in its own namespace:

- C: the `remote-host` binary, its admission table seeded with one key;
- A: the `baybo` gateway binary, so the gateway under test is the shipped entry point. The `seed_relay_binding` example (`crates/gateway/examples/`, behind `test-support`) first writes an approved relay binding (the device row and relay settings, and A's Noise static and relay node id in its vault) into the workspace, prints the phone's matching pairing record, then exits. The test reads A's link table through `baybo device status --json`;
- P: the ffi client, run from the `netns_phone` example, one command per stdin line, with the in-memory keychain backend (`test-support`) seeded with that record.

The test lives in `app/ios/ffi/tests/netns_matrix.rs`; every namespace and link is built per run, so no conntrack state crosses runs.

```
 ns:a ── ns:nat-a ──┐                        ┌── ns:nat-p ── ns:p
 10.0.1.2 · 2001:2:0:a::2   ns:inet (bridge)  10.0.2.2 · 2001:2:0:b::2
                    ├──── 198.18.0.0/24 ─────┤
                    │     2001:2::/64        │
                    └──────── ns:c ──────────┘   198.18.0.10 (HTTP/WS + UDP rendezvous)
 same-LAN variant: ns:p on ns:a's bridge (10.0.1.3)
 open profile: the side's LAN is public and routed (198.18.1.2 for A, 198.18.2.2 for P)
```

Every NAT namespace sets `net.netfilter.nf_conntrack_udp_timeout=30` and `nf_conntrack_udp_timeout_stream=30`, both per namespace, so that the keepalive is exercised. Every profile drops unsolicited WAN traffic to the NAT box before conntrack confirms it, as a consumer router does:

```
iptables -A INPUT -i wan -m conntrack --ctstate NEW -j DROP
```

Without that rule, an early inbound punch pins a conntrack entry, and masquerade then remaps the host's port. The rules are iptables (its nftables backend on current distributions), which needs only the `xt_*` matches every distribution kernel ships. Each NAT box applies one profile to IPv4:

- `open`: routed; no NAT, no filter.
- `cone` (endpoint-independent mapping and filtering): `-j SNAT --to-source <wan>`, which preserves the port, plus `-i wan -p udp --dport 1024:65535 -j DNAT --to-destination <host>`.
- `port-restricted` (endpoint-independent mapping, address-and-port-dependent filtering): `-j MASQUERADE` (conntrack filters replies to the exact remote).
- `symmetric` (address-and-port-dependent mapping): `-j MASQUERADE --random-fully`.
- `v6-firewall`: IPv6 is routed but not NATed. `ip6tables` drops new inbound connections from the WAN interface, so forwarded traffic is accepted only when established or related.
- `udp-blocked`: `-p udp -j DROP` on forward, both families.

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
- `ipv6-relay`: both sides reach C's HTTP/WS signalling over IPv6, with IPv4 UDP rendezvous still enabled → `Ipv6` (the namespace fixture uses plaintext HTTP/WS);
- `udp-blocked` on either side → `Relay`;
- C without `UDP_PUBLIC_ADDR` → A `open` gives `Ipv4`, and every other IPv4 cell gives `Relay`;
- `tc netem loss 5%` on both WANs, port-restricted × port-restricted → `Ipv4Punched`.

P IPv6-only behind a NAT64 without CLAT is not in the matrix yet: it needs `jool`, which neither CI's runner nor a stock distribution kernel ships. That path, and HTTPS on the real relay, remain real-device checks.

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
11. **`baybo device status`** matches the app's Connection row, except that a hairpinned "Direct · IPv4 (punched)" carrier is listed as `lan` (see *Link table*).

## Deploying C (operators)

- **Set `UDP_PUBLIC_ADDR`** to an IPv4 literal plus port, or to a hostname plus port whose A records point at C. Clients use only the name's IPv4 results, every one of which must be `Public`, so an AAAA record is ignored. The hostname must be **DNS-only**: a CDN-proxied name cannot carry UDP and would hide the NAT mapping. It is separate from the relay hostname, which may stay behind a CDN. If it is unset, the rendezvous is off, but offers are still forwarded.
- **Open the UDP port inbound** (`.env` `UDP_PORT`, default 7777/udp) in the host firewall and in any provider security group. C needs no IPv6 for this feature.
- **Preserve the source address** when publishing the port from a container. The compose file publishes `0.0.0.0:${UDP_PORT}:${UDP_PORT}/udp` through the kernel NAT path (the default iptables/nftables DNAT publishing). A userland proxy rewrites the source to a private bridge address. C then rejects every registration and logs `udp_source_not_public`.
- **On a multi-homed host running C with host networking,** set `UDP_BIND_ADDR` in `.env` to the public-facing IPv4 address plus `UDP_PORT`; the compose file passes it through. Replies must leave from the address the clients sent to, because clients drop a `Peer` from any other source. With the default published port, it stays unset: the kernel NAT already answers from the address the client sent to.
- The existing `RELAY_IP_*` per-IP limits cover `POST /direct`. The per-client, per-node and pending-punch limits are fixed constants.

## Invariants

1. **The relay is never gated.** No dial waits on a probe, and the chat dial path never uses a carrier. Rotation is the only way a chat leg reaches a direct carrier, and it starts only when the chat is idle.
2. **Every carrier session runs `DirectOpen`, then Noise IK between the pairing statics, confirmed by P, then the same responder as a relay leg.** A resolves the device only by its static key, and counts the session as that device's only once the confirmation decrypts.
3. **Pairing is relay-only.**
4. **A's carrier runtime lives in the binding scope**, which exists only while the binding resolves to one `Ready(settings)`; a control redial does not restart it. Stopping it drops every carrier session's task, pre- and post-authentication, before it closes the endpoints.
5. **One stable UDP socket per family per side.** Registration, punches and QUIC share it. A's lives for the runtime and is bound again only after a failed bind or a persistent socket error; P's lives for a probe and its carrier. Receive errors back off and do not tear down the runtime.
6. **Every endpoint sets `grease_quic_bit(false)`**, so its peer never clears the fixed bit toward it, and every probe datagram's first byte has `0x80|0x40` clear. The demux splits GRO buffers into segments and is built on `quinn_udp`, so replies keep the targeted source address.
7. **A admits an `Incoming` only when its source IP is in a live punch's allowed set,** retries an unvalidated address, pins its QUIC transport limits and disables connection migration. Everything else is `ignore()`d, and the demux keeps from quinn the packets it would answer before admission (a version other than v1, an Initial with a short destination connection ID).
8. **Stream authentication is spawned, never inline in `accept_bi`.** A connection gets `FIRST_STREAM_DEADLINE`, counted from `Incoming::accept()`, to authenticate a stream.
9. **Caps are named by what they count and keyed by `source_key`.** The per-connection stream limit is sized above the app's leg fan-out.
10. **A and P send probe and QUIC datagrams only to sealed host candidates, a `Public` IPv4 rendezvous address from `resolve_public_v4`, and a latched `Public` IPv4 `Peer` address,** and never to one of their own interface addresses. The `Peer` must come from the resolved rendezvous address, verify under the side's role key and belong to a punch the side took part in. Each side latches the first valid `Peer` per punch. An authenticated punch admits a source at A but never makes it a target.
11. **Sealing:** an all-zero DH output is rejected; the keys are directional and `Zeroizing`; every seal uses a fresh random 24-byte nonce; the AAD binds the node id, plus the punch id for answers; each message kind has one fixed plaintext length; any AEAD, length, version or cap failure rejects the whole set; P requires the `offer_id` echo; A enforces `OFFER_MAX_AGE`, the process's `started_at_ms` and the process's replay cache.
12. **Secrets:** the direct token is minted from a CSPRNG per binding runtime and travels inside sealed answers and, under QUIC's TLS, in the `DirectOpen` preface. It is zeroized on drop, never printed, and compared in constant time. Punch ids and rendezvous keys are minted from a CSPRNG at C, per punch and per role; a key travels only over TLS, is zeroized on drop and never printed, and the device's key can never register the gateway's role. `offer_id` is minted from a CSPRNG at P.
13. **C:** a malformed capability or report never closes control; direct state is per punch and has a TTL; (PR1) `MAX_RELAY_NODE_ID_BYTES` is enforced wherever a node id becomes a map key; the direct path never causes a control redial.
14. **C's UDP:** at most one reply per `Register`, only to its sender, never longer than it; only for a live punch, a `Register` that verifies under the role's key and the role's latched source, and only such a `Register` spends the punch's datagram budget; IPv4 sources only. `UDP_PUBLIC_ADDR` is configured separately from the relay hostname.
15. **P's probe state:** probes are single-flight per binding; the failure cache is per `(binding, network)` and counts only failed probes and unsolicited early carrier deaths; `network_changed` ignores duplicates and secondary-interface changes and otherwise retires the carrier synchronously; the background barrier suspends the carrier and `.active` re-proves it; a result with a stale epoch is discarded; pair and forget clear carrier state infallibly; a carrier is evicted by closing its connection (`stable_id`), never the endpoint.
16. **P's legs:** every carrier transition invalidates the API leg pool; a failed carrier leg dial falls back to the relay for that leg at once, and only connection-level evidence retires the carrier; while a rotation is in flight, sends are held and the relay leg's `PumpEnded` waits for the rotation's outcome.
17. **Chat legs:** A stamps each chat leg with a sequence when it opens it, and a leg installs only over one opened before it; a relay chat leg installs only at its first decrypted transport message and closes uninstalled after `FIRST_TRANSPORT_MESSAGE_DEADLINE`.
18. **Config rules live only in `validate.rs`, and address classes only in `AddressPolicy`.** A never enumerates interfaces when no direct socket is bound.

## Rejected

- **Direct-first dialing with a per-leg budget.** Every leg would pay up to the budget on any network where direct fails, and the relay would stop being the baseline. Relay-first costs nothing when direct fails and one background probe when it works.
- **A TCP carrier.** TCP cannot be hole-punched, so it reaches A only on a LAN, through an inbound-open IPv6 firewall, or through a port the operator forwards and advertises. Its one gain is a network that blocks UDP, where the relay already works. It would cost a plaintext `DirectOpen` token and Noise msg1 that an on-path host can replay, a pre-authentication permit scheme for a listener every source can reach, and a second carrier model on P. A future TCP carrier comes with a `DIRECT_PROTOCOL_VERSION` bump, since it changes `GatewayAnswer`.
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

**PR1: protocol, C and gateway** (implemented; inert without the app):

- **Protocol and crypto:** the wire types and constants, including `MAX_RELAY_NODE_ID_BYTES`; `AddressPolicy` with `active()`, `for_tests()` behind `test-support`, `source_key` and `resolve_public_v4`; the `ProbeDatagram` codec. In `device-proto`: `DeviceSealer` and `GatewaySealer`, punch tags, `DeviceOffer`, `GatewayAnswer`, `OfferId` and `CertHash`; the crate gains `base64`, `hmac` and `subtle`, and `subtle` joins the root `[workspace.dependencies]`.
- **`crates/carrier`** (new, package `carrier`, `doctest = false`): `DemuxSocket` (GRO segmentation, dropping the QUIC packets quinn would answer before admission, `try_send` for probe datagrams, the receive backoff), the QUIC transport and endpoint config (pinned limits, `grease_quic_bit(false)`, `ServerIdentity`, the pinning certificate verifier), `DirectOpen` framing with the cancel-safe `FrameReader`, `PunchBurst` pacing, the rendezvous `Registration` and `PeerLatch`, and `CarrierKind`.
  - The config builders take each side's rustls `CryptoProvider`, so the crate names none: A passes aws-lc-rs (the gateway already enables rustls's `aws_lc_rs`), and P will pass ring. The crate enables quinn's `rustls-ring`, which is what brings quinn's rustls integration into existence, so it builds and lints on its own; ring and rustls's ring support are already in both the root and the iOS graphs, so this adds no crate to either lockfile, and aws-lc-rs never enters the app. rcgen generates A's key with ring too. The crate's `test-support` feature adds `DemuxSocket::inject_recv_errors` for the gateway's rebind test.
- **C:** `POST /direct` with the per-client and per-node budgets and the per-client punch and direct-control caps; the punch registry and its in-flight rules; control-report parsing; the `ControlRegistry::register` node-id check; the UDP rendezvous (`UDP_PUBLIC_ADDR`, `UDP_BIND_ADDR`, `DEFAULT_UDP_BIND_ADDR`); the edge label; `docker-compose.yml` and `.env.example` (`UDP_PUBLIC_ADDR`, `UDP_PORT`, the optional `UDP_BIND_ADDR`); `DEPLOY.md`.
- **Gateway:**
  - the binding scope in `relay_content::run` and the `RelaySettings` identity fields;
  - the carrier runtime: stable sockets, QUIC endpoint, admission, authenticated-punch verification, punching;
  - the outbound `ControlReport` path in `pump_control`, and the capability in the hello;
  - on the responder seam, the handshake confirmation (`BinarySink::CONFIRMS_HANDSHAKE`) and the `authenticated` hook; the API tunnel polls a forwarded request's router inside its session, so a dropped session drops the handler;
  - `DeviceLinks`, `GET /v1/mobile/links` and `baybo device status`. The route is registered in the OpenAPI doc under a `mobile` tag; `docs/openapi.json` is regenerated with `UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync`, and `app/web/src/api/schema.d.ts` from it with `pnpm --filter baybo-web gen:api`. The rule for dialing the admin listener from the same host moves from `crates/baybo/src/gateway_client.rs` to `baybo_gateway::config::admin_dial_addr`, which `baybo tui`, `baybo prompt` and `baybo device status` share;
  - config in `crates/config/src/{gateway,validate}.rs`.
- **Tests:** unit tests plus the `relay_e2e.rs` direct cases. `.github/workflows/ci.yml` gains a `remote-host` job (fmt, clippy `-D warnings`, nextest in the `remote-host/` workspace, whose own `remote-host/.config/nextest.toml` fails a hung test instead of wedging the job), path-filtered on `remote-host/` and `rust-toolchain.toml`. The root workspace excludes `remote-host/`, so without this job no CI run executes the protocol and C tests. It also gains a `gateway-macos` job on `macos-26`, path-filtered on the carrier code (`crates/carrier/` since PR2 moved the enumeration there, and `crates/gateway/src/channel/carrier/`), that runs the interface tests: the IPv6 address flags have an Apple-only reader, whose ioctl request is pinned at compile time to the SDK's value, and no other CI job runs Rust tests on an Apple platform.
- **Docs:**
  - `companion.md`: "Reaching a NAT'd gateway" (content may bypass the relay on a direct carrier; pairing stays relay-only), the binding scope, and the E2E's direct cases;
  - `relay-push-security.md`: what C may see, the C can / cannot additions, and Claim 5;
  - `remote-host/DEPLOY.md`: the routes table and UDP setup, with the knobs in `.env.example` and `docker-compose.yml`;
  - `config.md`; `gateway.md` (the `carrier/` and link-table files, `GET /v1/mobile/links`, and that a containerized gateway needs host networking for direct UDP);
  - `cli.md` (the `device` row gains `status`);
  - `testing.md` (the protocol and relay `test-support` fixtures, and running the `remote-host` workspace's tests);
  - `relay-api-tunnel.md` (the forwarded request's router is polled inside the session);
  - the NAT hole-punching entry in `docs/roadmap.md`, and this document's status in `docs/modules/README.md`.

  `.github/workflows/ci.yml` changes only by the `remote-host` and `gateway-macos` jobs: no `app/ios` crate depends on `crates/carrier` in PR1, so the iOS filters stay as they are until PR2.

**PR2: the app**:

- **ffi:**
  - `WsStream` is generalised to a leg socket (`app/ios/ffi/src/transport/socket.rs`: a WebSocket, or a carrier stream framed by `carrier::framing`) across `transport/{mod,pump}.rs`, `relay/tunnel.rs` and `relay/chat.rs`. On a carrier session every handshake ends with P's confirmation, one empty transport message sent right after msg2; the relay leg's handshake is unchanged. `relay/dial.rs` stays WS-only.
  - `app/ios/ffi/src/relay/carrier/`: the pure state machine (`state.rs`: single flight, the epoch, the per-network failure cache, suspension, cool-off), the network fingerprint and key (`network.rs`), the prober (`probe.rs`), the carrier handle and leg dials (`quic.rs`), and the hub that runs them (`mod.rs`). `carrier` joins `app/ios/Cargo.toml`'s `[workspace.dependencies]` with `quinn`, and `tracing` with its `log` feature so the carrier crate's events reach the app's log. `.github/workflows/ci.yml`'s `IOS_DEPS` and `ios_native` filters widen to `crates/(wire|device-proto|model|carrier)/`, and `crates/carrier/**` joins the `ios-sim` job's ffi cache key.
  - The interface enumeration moves from the gateway to `crates/carrier/src/interfaces.rs`, shared by both sides, with temporary IPv6 addresses reported apart from unusable ones.
  - In the supervisor: the active-turn set and the rotation transition, with its commit gate (`RotationGate`), held sends and the held relay death. Every carrier transition invalidates the API leg pool.
  - In `app/ios/ffi/src/lib.rs`: `network_changed`, `set_carrier_sink` and `CarrierSink`, and `carrier_background` / `carrier_foreground` for the `.background` barrier and `.active`. Pair and forget clear carrier state.
  - Behind the ffi's `test-support` feature: an in-memory keychain backend for host builds, so the netns client can be seeded with a `PairedRecord`, the rotation-hold knob, and the protocol's test address policy.
- **Swift:** `app/ios/App/Core/PathMonitor.swift`, `ConnectionStore.swift` (the carrier sink's store) and the Settings Connection row. `NSLocalNetworkUsageDescription` was already present.
- **Tests and docs:** the netns matrix, its CI job, the gateway's `seed_relay_binding` example and the ffi's `netns_phone` example; `app/ios/docs/connection.md`.
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
