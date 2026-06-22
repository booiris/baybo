# mobile companion — phase roadmap

The iOS (then Android) companion for Aura, built with **Tauri**. Architecture
reference: [`mobile-remote-host.md`](../../mobile-remote-host.md). Each phase below
has its own planning doc in this folder; detail firms up as a phase is scheduled,
so later-phase docs are intentionally lighter than `phase1`.

> **Naming (used in every doc):** **A** = the user's *aura gateway* (runs the
> agent, holds the transcript); **C** = *`aura-remote-host`* (operator-run shared
> infra: push + relay + rendezvous + admission + dashboard, holds the `.p8`);
> **P** = the iOS/Android *app* + its push extension. The E2E ends are **P and A**;
> C and Apple/Google see only ciphertext + routing metadata.

| Phase | Theme | One-line scope | Doc |
|---|---|---|---|
| **1** | Pairing + push (receive-only) | QR/SPAKE2 pairing, blind encrypted-preview push + NSE, self-pull + minimal render | [phase1.md](phase1.md) |
| **2** | Two-way client (usability) | Message **send** (in-app composer), web-chat-parity UI, notification mark-read, push-key rotation, background-job push | [phase2.md](phase2.md) |
| **3** | Production & distribution | Production APNs, TestFlight → App Store, C operations (admission rotation, rate caps, abuse, SLOs) | [phase3.md](phase3.md) |
| **4** | Connectivity (take content off the relay) | NAT hole-punching + relay-fallback (iroh / DCUtR); content bandwidth off C | [phase4.md](phase4.md) |
| **5** | Android | `app/mobile/android` reusing the shared core; FCM push path in C; multi-platform push | [phase5.md](phase5.md) |

## Ordering rationale

- **P3 (production) before P4 (hole-punch)** — resolved. Early users are few and
  relay bandwidth is cheap (see `phase1` cost estimate: thousands of instances ≈ a
  few $/month). Ship to real users on relay-only first; build hole-punching only
  when bandwidth is a **measured** cost, not a speculative one. Hole-punch is a
  cost optimization, not a prerequisite for shipping.
- Each phase builds on the prior's protocol + crates; none re-opens a phase-1
  **architectural** decision (Tauri, no-UniFFI, `aura-wire` + `aura-device-proto`,
  blind C, SPAKE2-over-rendezvous, multi-gateway). One **scope** choice is revisited:
  P2 re-enables `SubagentNotification` push, which phase 1 deliberately deferred — a
  filter-scope expansion, not an architecture change.

## Deferred backlog → phase mapping

Everything `phase1` lists as out-of-scope lands here: message send + polished UI
(P2), push-key rotation (P2), background-job/SubagentNotification push (P2),
production APNs + App Store + C multi-tenant ops (P3), NAT hole-punch + iroh (P4),
`app/mobile/android` + FCM (P5), richer-than-4 KB preview via NSE fetch (P3, if
needed).
