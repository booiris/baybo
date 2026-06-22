# mobile phase-4 — connectivity (take content off the relay)

> **Status: planning (roadmap altitude).** Builds on [phase3.md](phase3.md);
> architecture reference [`mobile-remote-host.md`](../../mobile-remote-host.md).
> A **cost optimization**, not a prerequisite — scheduled *after* production
> ([roadmap rationale](README.md#ordering-rationale)) so it's driven by **measured**
> relay bandwidth, not a guess.

## Goal

Take the open-ended cost identified in phase 1 — **content bandwidth, 100 % on C in
always-relay** — off C for the common case, by establishing **P2P-direct** content
paths via NAT traversal, keeping C's blind relay only as the genuine fallback.

This realizes the connectivity model resolved in the design interview: *P and A
both connect to C; C coordinates; direct when possible (now via hole-punch, not
just configured reachability), relay otherwise.*

## Scope

**In:**

1. **NAT hole-punching + relay fallback.** C's `relay` component gains
   NAT-traversal coordination (STUN-like address discovery + simultaneous-open /
   DCUtR-style hole punch). The phase-1 "direct-reachability probe first" skeleton
   becomes "**hole-punched-direct first**, relay fallback."
2. **Substrate decision: iroh vs. bespoke.** The interview deferred this. **iroh**
   provides both a relay (its DERP-derived relay servers) and hole punching — *its
   own* discovery/hole-punch path, **not literally libp2p DCUtR** — in one Rust lib,
   fitting C + `aura-mobile-core` directly. Reconciliation with our trust layer is
   **not symmetric**: iroh authenticates endpoints by **ed25519 `NodeId`** over
   QUIC/TLS, while our identities are **X25519 static keys** anchored by the SPAKE2
   pairing TOFU — there is no free "mapping" between them. **Default: layer our
   existing Noise over iroh's QUIC streams** (iroh as a dumb authenticated-transport
   / NAT substrate; our Noise stays the sole E2E, trust model unchanged). Adopting
   iroh's *own* encryption as the E2E is a **flagged alternative** requiring a fresh
   MITM / identity-binding security review (it would replace the SPAKE2-anchored
   TOFU). Bespoke keeps our Noise clean but means building hole-punch ourselves.
3. **Relay bandwidth quotas + accounting at scale.** Built on the per-instance
   **metering that landed in P3** (which gates whether P4 starts at all). P4 adds
   quota enforcement + scaled accounting, **not** the basic metering.

**Out:** anything that re-opens the blind property or the E2E ends (P↔A).

## Key decisions / approach (to decide when scheduled — driven by data)

- **iroh vs. bespoke** — the load-bearing choice. iroh absorbs the NAT-traversal
  complexity but brings the ed25519-`NodeId`/QUIC identity model; bespoke keeps our
  own Noise but is more to build.
- **Where the E2E lives — default Noise-over-iroh.** Keep **our Noise** as the sole
  E2E layered over the chosen transport. Adopting iroh's node encryption is a flagged
  alternative needing a fresh "C blind, MITM blocked once paired" proof and a concrete
  X25519↔ed25519 binding story — not a hand-wave.
- **iOS lifecycle.** Hole-punched P2P only matters when the app is **foreground**
  (a backgrounded/killed phone can't hold a P2P link) — push still wakes it, then
  it establishes connectivity on open. Scope hole-punch to foreground content
  sessions; relay remains for everything else.
- **Trigger.** Only build this once relay bandwidth is a measured cost (the metering
  from item 3 is the gate).

## Dependencies

- Phases 1–3 landed; the **per-instance relay bandwidth metering owned by P3**
  provides the data that justifies starting P4.
- `aura-mobile-core` + C `relay`: the phase-1 "prefer-direct" seam is the
  integration point — P4 swaps the reachability probe for full NAT traversal.
- No change to pairing, push, the device registry, or the blind invariants.

## Landing slices

1. **Confirm go/no-go** from P3's bandwidth metering (real data, not a guess).
2. **Substrate spike** (iroh vs. bespoke) → resolve the trust-layer reconciliation.
3. **Hole-punch path + relay fallback** behind the existing prefer-direct seam.
4. **Quotas / accounting** at scale.

## Open questions

- iroh vs. bespoke, and (if iroh) Noise-over-QUIC vs. iroh-native encryption + the
  X25519↔ed25519 binding.
- The bandwidth threshold that triggers building this.
- Foreground-only scope confirmation for P2P.

## Related

- [README.md](README.md) — why this follows production.
- [phase1.md](phase1.md) — the connectivity model, the "direct-first" seam, the
  bandwidth cost analysis this addresses.
- [`mobile-remote-host.md`](../../mobile-remote-host.md) — relay/rendezvous, the
  blind byte-pipe, the deferred iroh note.
