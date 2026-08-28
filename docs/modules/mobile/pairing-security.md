# Mobile companion — pairing security

How device pairing stays safe against a **malicious or compromised relay (C)**.
This is the threat model + cryptographic design behind the
[mobile companion](companion.md); read that first for the roles (A =
gateway, C = remote host, P = phone) and the wire shapes.

> Not to be confused with [`pairing.md`](../pairing.md), the *channel*-pairing gate
> (a 6-char operator code for sidecar-routed inbound). That code **is**
> operator-facing and not secret. Device pairing is the opposite posture — it
> carries a high-entropy secret that must never leak. Same word, different
> subsystem.

## Threat model

The default pairing path runs over a **public relay** (`wss://proxy.baybo.space`)
with a shared `guest` admission key, so **"the relay is hostile" is the default
trust position, not a corner case**. C must be assumed to be an active
man-in-the-middle on the rendezvous: it sees every byte, can drop/inject/reorder,
and can open its own legs. The one thing C must never be able to do is
**complete a pairing as either side** — i.e. substitute its own keys and own the
post-pairing channel.

The phone pins nothing about the gateway out of band: standard rustls CA roots,
and the gateway's static key is learned **in-band** during the handshake. So the
handshake itself is the only thing standing between a hostile relay and a full
takeover — there is no downstream check that would catch a substituted key.

### Why the old SPAKE2 design failed

The previous design used a single **pairing code** for two contradictory jobs:
the SPAKE2 password **and** the relay routing key (`/pair/{host,join}/{code}`).
Because C must read the code to route, it held the SPAKE2 password by
construction — so SPAKE2's only property (one online guess) evaporated. C could
run SPAKE2 with each side, derive both channel keys, open both sealed frames,
substitute its own Noise static into `DeviceHello` / `GatewayWelcome`, and steal
the active bearer token — a persistent MITM. The 6-digit confirm code didn't
help: it folded only the SPAKE2 secret `K` (not the exchanged statics), had no
commitment phase, and was ~20 bits — C, as the last mover, could grind its
ephemeral until both screens showed the same number, under no time pressure.

The fix splits the one code into two values and replaces SPAKE2 with an
authenticated Noise handshake.

## The two values

| Value | Entropy | Who sees it | Role |
|---|---|---|---|
| **`rendezvous_id`** | 122-bit UUIDv4, **public** | C (it routes on it), the QR | broker key, `/pair/{host,join}/{id}` param, slot key, first-frame selector |
| **`secret`** | **256-bit CSPRNG**, never on the wire | **only** the QR (out of band) | the Noise **PSK** |

C only ever sees the public `rendezvous_id`. The `secret` travels **only** in the
QR — never over the relay, never in a URL, never logged, never persisted past the
slot. Lacking the PSK, C cannot complete the handshake with either side, so it is
reduced from MITM to **denial-of-service**.

The QR encodes both:
`baybo://pair?h={endpoint}&r={rendezvous_id}&s={secret}&k={remote_api_key}`.

## The handshake: `Noise_XXpsk0_25519_ChaChaPoly_SHA256`

App = initiator, gateway = responder (preserves "app speaks first"). `XX` is the
pattern that **exchanges both statics in-band** — exactly today's model (neither
side knows the other's static ahead of time). `XXpsk0` mixes the QR secret as a
PSK **before the first ephemeral**, which *authenticates* that otherwise-anonymous
exchange:

```
        app (P, initiator)                       gateway (A, responder)
msg1 ─► Hello { rendezvous_id, e }    claim the in-memory slot by rendezvous_id
        (psk pre-mixed, then e)       (it holds the secret),
                                      build the XXpsk0 responder
msg2 ◄─ HandshakeReply { msg }        (e, ee, s, es) — app now holds the gw static (in h)
msg3 ─► HandshakeFinal { msg }        (s, se) — app static + the DeviceHello body as payload
        ── both in transport mode, share the final handshake hash h ──
        confirm_code = HKDF(h)        confirm_code = HKDF(h)
        show on phone; user accepts   publish to slot; operator confirms
msg4 ─► Sealed( DeviceConfirm )       (transport msg)
msg5 ◄─ Sealed( GatewayWelcome )      (transport msg, only after the operator confirms;
                                       carries the active auth_token + gateway_push_pubkey)
msg6 ─► Sealed( DeviceDelegation )    (transport msg; the device signs the welcome's
                                       gateway_push_pubkey, authorizing it to manage the
                                       device's push binding at C — see relay-push-security.md)
```

`DeviceHello` carries only `device_id` as the **msg3 payload** (authenticated by
the app static). Push-token registration is deliberately outside pairing and
uses the authenticated device API after the binding exists. The statics themselves are
XX handshake tokens, not payload fields — so neither `DeviceHello` nor
`GatewayWelcome` carries a `static_pubkey`. `DeviceConfirm` / `GatewayWelcome` /
`DeviceDelegation` are transport-mode messages (snow's implicit nonce).

(Implemented in `crates/device-proto/src/psk_pair.rs`: `PskHandshake` /
`PskTransport` over `snow`. SPAKE2 — `pake.rs` and the `spake2` dep — is deleted.)

### Why XX, not NK

`NK` (the original sketch) would require the app to know the gateway's static
**before** the handshake → the gateway static would have to ship in the QR and the
app would persist it from there. That is a *different, larger* model change and is
**not** done. (`NKpsk0` / pin-the-gateway-out-of-band remains a possible future
upgrade; out of scope here.) `NN` would also authenticate via the PSK, but the
handshake hash wouldn't cover the statics, forcing `HKDF(h ‖ statics)` for the
confirm code and leaving reflection to be patched via role labels. XX is cleaner.

## The load-bearing invariant: the secret must be high-entropy

A Noise PSK is **not** a PAKE. Against an *active* participant (the relay), one
captured transcript is enough to mount an **offline dictionary attack** on the PSK
(try candidate → recompute the chaining key → check the AEAD tag). SPAKE2's
"one online guess, no offline attack" property — what made a short human code safe
— **does not survive the switch**. Therefore the secret is:

- CSPRNG-generated, **256 bits / 32 bytes** (snow's PSK width, no expansion);
- carried **only** in the QR, with **no typeable form** (a short/typeable secret
  would be *worse* than the old design — offline-cracked instead of
  online-one-guess).

This is encoded as a code-level invariant: `PairingSecret` is a 32-byte newtype
with a CSPRNG constructor + `from_bytes` only (no string constructor), `Zeroize`,
and a redacted `Debug`. This single requirement is the spine of the whole design.

## Prologue binding

Both ends build an identical, **canonical, versioned, length-prefixed** prologue
and feed it to the snow `Builder`:

```
prologue = v1 ‖ rendezvous_id ‖ endpoint ‖ role-labels
```

It is authenticated (folded into `h`) but **not secret** (the PSK stays out of
it). A one-byte disagreement aborts the handshake. This buys, for free:

- **anti-splice / anti-cross-binding** — C can't wire app-of-pairing-X to
  gateway-of-pairing-Y (mismatched `rendezvous_id`), nor cross-bind two relays
  (mismatched `endpoint`), even if a secret were ever reused;
- **role binding** — blocks reflection / role confusion (replaces SPAKE2's
  `ID_APP` / `ID_GATEWAY` labels);
- **downgrade resistance** — C can't swap the bound `endpoint`, and the app
  refuses if the gateway static `s` is absent.

## Confirm code = channel binding

The short human-comparable confirm code is derived from the **Noise handshake
hash** `h` (`kdf.rs`), not from the secret. `h` commits to the whole transcript —
the prologue, both ephemerals, the PSK mix, **and both statics** (XX) — so a
matching code proves a single, live, untampered session and is **not grindable or
precomputable**. The two-sided human confirm is kept but repositioned as
**authorization** (the operator decides "pair this device") and as the backstop if
the QR secret leaks (shoulder-surf / forward); the operator prompt defaults to
**no**. The per-device 32-byte `push_key` derives from `h` the same way
(`PUSH_KEY_INFO`).

## Secret hygiene

The secret lives only in the in-memory `DevicePairingSlot` (zeroized on drop) —
never a plaintext column, never the durable `DeviceRow`, never `device list`,
`GatewayWelcome`, `PairedSummary` (the uniffi record the iOS app renders after
pairing), or any log. Only `rendezvous_id` (public) reaches those places — it is
what `device list` and the durable row record. Observability logs at most the
public `rendezvous_id` (device ids are logged hashed via `short_hash`), never the
secret or the confirm code.

## Design constraints

- **Fail closed.** No PSK / wrong PSK / mismatched prologue → the handshake
  aborts, no token. An absent `s=` in the QR → the app refuses to pair (no silent
  downgrade).
- **Fresh + single-use.** A new secret + rendezvous id per pairing, consumed and
  zeroized on `stage`, never reused. XXpsk0's `ee` gives forward secrecy even if
  the secret later leaks; single-use keeps any leak window to one rendezvous.
- **Relay-only.** Pairing runs exclusively over the relay; there is no direct/LAN
  pairing route. The prologue binds `endpoint`, which is what prevents cross-relay
  binding.
- **Relay admission (`remote_api_key`) is orthogonal.** It is C's *own* per-tenant
  anti-abuse allow-list on a multi-tenant host (a per-key connection cap, two-level
  bandwidth limits, and optional per-key expiry — the default `guest` key is an
  ordinary admitted key, not a special class; resolved through one shared
  `Admission::resolve` seam), **not** protection against a hostile C — the PSK is
  what defends the app↔gateway channel. A leaked `remote_api_key` only lets someone
  burn C's quota (rotated control-plane-side, relay-agnostic); it can never MITM a
  pairing. Do not conflate.

## Residual DoS (availability only)

A hostile relay can always deny service, and the public `rendezvous_id` makes
**leg-stealing** reachable by any holder of it (a QR photographer, or any holder
of the shared `guest` key): join `/pair/join/{id}`, `try_match` steals the
gateway's parked host leg, fail the PSK handshake, grief pairing. This is
availability only (the PSK still blocks MITM). Mitigations:

- the gateway **re-parks its host leg immediately** on a PSK-auth failure (no
  backoff), so a leg-stealer can't impose latency on the real app's retry;
- a **per-rendezvous `/pair/join` rate limit** (the relay warns once per window
  when it trips);
- the app **times out** the handshake reply, so a relay can't pin it open;
- the typeable QR fallback is removed — on a render failure the command errors out
  asking for a wider terminal, never a shortened secret and never written to disk.

## Out of scope

- **NKpsk0 / gateway-static-in-QR** — a possible future out-of-band gateway pin;
  not needed to close the MITM.
- **Dropping the shared `guest` admission key** — the PSK already defeats MITM, and
  the re-park + join rate-limit cover the residual griefing.

## Related

- [`companion.md`](companion.md) — the companion architecture.
- `crates/device-proto/src/psk_pair.rs` — the XXpsk0 state machine + `PairingSecret`.
- `crates/device-proto/src/kdf.rs` — confirm code + push key from `h`.
- `crates/gateway/src/channel/device_pair.rs` / `relay_pair.rs` — the A-side
  `drive()` + the relay host manager (re-park on PSK failure).
- `crates/pairing/src/device_service.rs` — `mint` (returns rendezvous id + secret)
  / `stage` + `approve_staged` (consume the slot; store only `rendezvous_id`).
