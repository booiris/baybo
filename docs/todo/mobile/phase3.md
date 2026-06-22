# mobile phase-3 — production & distribution

> **Status: planning (roadmap altitude).** Builds on [phase2.md](phase2.md);
> architecture reference [`mobile-remote-host.md`](../../mobile-remote-host.md).
> This is the "ship to real users at scale" phase: real production push, App Store
> distribution, and hardening **C** (`aura-remote-host`) into operable multi-tenant
> infra. Ordered **before** hole-punch ([phase4.md](phase4.md)) on purpose — see the
> [roadmap rationale](README.md#ordering-rationale).

## Goal

Move from "works on a dev build against sandbox APNs in the Simulator/one device"
to "any self-hoster's users get notifications from the published app," with C run
as accountable, rate-limited, observable infrastructure.

## Scope

**In:**

1. **Production APNs.** Drive `api.push.apple.com` (production) alongside
   `api.sandbox.push.apple.com` — the **same `.p8`**, host chosen per device's
   tracked `apns_env`. But `apns_env` is **not a free routing switch**: APNs device
   tokens are **environment-bound** (a sandbox token is rejected by the production
   host and vice versa), and the env is fixed by the build's `aps-environment`
   entitlement at provisioning time with **no runtime API to read it back**. So the
   load-bearing work is (a) **determining `apns_env` correctly at registration**
   (from the build channel / embedded entitlement — TestFlight/App Store ⇒
   production, dev-signed ⇒ sandbox — never guessed), and (b) **distinguishing an
   env-mismatch `400 BadDeviceToken` from a genuinely dead token**, so the pruner
   doesn't unbind a live device. The app must surface its build env explicitly to A
   at registration.
2. **App Store distribution.** TestFlight → App Store review + release. Bundle ID,
   `aps-environment: production` entitlement, App Group + Keychain access group,
   and the NSE all signed for distribution. Privacy nutrition labels reflect the
   blind design (no content leaves the phone to C).
3. **C multi-tenant operations** (the items phase 1 cut to "minimal"):
   - **Admission-key rotation** + lifecycle (issue / rotate / revoke per-instance
     keys without downtime).
   - **Per-device and per-instance rate caps** (Home-Assistant-style ~daily caps
     as precedent) + abuse detection/throttling on the relay and push ingest.
   - **Per-instance relay bandwidth metering** — the measurement that **gates P4**
     (per the README's P3-before-P4 rationale): **P3 owns the metering**, P4 owns
     quotas + the hole-punch decision built on it.
   - **`.p8` rotation + key management** (Key ID rollover without dropping pushes).
   - **Token pruning at scale** (`400`/`410` handling honoring the `410` timestamp).
   - **Dashboard maturation**: instance health, push success/failure, APNs token
     health, relay connection/bandwidth stats, rate-limit hits — **metadata only,
     still blind**.
   - **Observability / SLOs**: hashed-id logging end to end, push-delivery and
     relay-availability SLOs, alerting.
4. **Richer-than-4 KB preview (optional, if needed).** The NSE *fetches* in its
   ~30 s window (we operate the relay) instead of inlining — only if product wants
   a preview that won't fit the 4 KB payload. **Trade-off:** fetching reopens
   exactly the reachability dependency phase 1 designed out (the inline NSE is
   robust *because* it does no network and doesn't need A reachable in the window);
   the fetch rides C's relay and is best-effort. That is why the **default stays
   inline** (reliable).

**Out:** NAT hole-punch (P4); Android/FCM (P5).

## Key decisions / approach (to confirm when scheduled)

- **One `.p8`, two hosts — but env is bound, not chosen.** Never split keys per env;
  route by `apns_env`. The value is fixed by the build's entitlement at provisioning
  (no runtime read), so it must be derived at registration, not guessed (item 1).
- **Admission rotation = overlap window.** Old + new instance key both valid during
  rollover; gateways pick up the new key on config reload.
- **Rate caps are blind.** Caps key on `device_id` / `instance_key` (hashed in
  logs), never on content. Define the cap numbers + the over-cap behavior
  (drop + dashboard counter, not a content-revealing error).
- **Distribution gating.** Decide TestFlight cohort → public release criteria;
  App Store review notes must explain the blind-relay model proactively.

## Dependencies

- Phases 1–2 landed (the app is a real client; push works against sandbox).
- C's `push`, `admission`, `relay`, `dashboard` crates from phase 1 — hardened, not
  re-architected. The `.p8`-holding `push` binary stays independently deployable.
- The Apple Developer account + published bundle ID — **already required and
  acquired at phase 1 / M4**; what is *new* in P3 is **App Store distribution**
  (TestFlight cohort → public release) and the production `aps-environment`
  entitlement, not the account itself.

## Landing slices

1. **Production APNs path** (env routing + a TestFlight build delivering real
   pushes to a real device).
2. **App Store submission** (entitlements, privacy labels, review).
3. **C operations**: admission rotation → rate caps + abuse → `.p8` rotation →
   dashboard/observability/SLOs.
4. **(Optional) richer preview via NSE fetch.**

## Open questions

- Rate-cap numbers (per-device/day, per-instance/min) and over-cap behavior.
- TestFlight → public release gating criteria.
- Whether the richer-preview NSE-fetch path is needed at all in P3.

## Related

- [README.md](README.md) — why P3 precedes P4.
- [phase1.md](phase1.md) — the C components + per-device `apns_env` tracking.
- [`mobile-remote-host.md`](../../mobile-remote-host.md) — push, admission,
  dashboard, token pruning, the blind constraints.
