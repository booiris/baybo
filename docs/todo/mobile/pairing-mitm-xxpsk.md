# Device pairing — close the malicious-relay MITM (SPAKE2 → Noise XXpsk0)

Hardening for the iOS-companion **device pairing** on branch
`feat/mobile-companion`. Read [`phase1.md`](phase1.md) for the A (gateway) /
C (remote-host) / P (phone) roles and [`docs/modules/pairing.md`](../../modules/pairing.md)
for the channel-pairing gate (a *different* subsystem — do not conflate).

## Problem

Pairing is supposed to be safe against a malicious / compromised **relay (C)** —
the whole point of running SPAKE2 over the rendezvous was "C relays opaque blobs,
never learns the code or the key" (`crates/device-proto/src/pake.rs:1-12`). That
guarantee is **false as built**, and a relay operator can mount a full,
persistent man-in-the-middle.

Root cause: **the pairing code is used for two contradictory jobs at once.**

1. It is the **SPAKE2 password** — `Pake::start_app(code)` /
   `start_gateway(code)` (`crates/device-proto/src/pake.rs:32-50`,
   `Password::new(code.as_bytes())`).
2. It is the **relay routing key** — `/pair/host/{code}` and `/pair/join/{code}`
   (`remote-host/crates/protocol/src/relay.rs:13-14`); the relay matches the two
   legs on that path segment (`remote-host/crates/relay/src/serve.rs:154,198-204`
   → `RelayBroker::join`/`try_match`, `broker.rs:70`).

The relay **must** read the code to route, so it holds the SPAKE2 password by
construction. SPAKE2's only security property — C gets at most one online guess —
evaporates: C runs SPAKE2 with the app and again with the gateway, derives both
channel keys (`kdf.rs:43-48`), opens both sealed frames, **substitutes its own
Noise static pubkey** into `DeviceHello`/`GatewayWelcome`
(`crates/gateway/src/channel/device_pair.rs:115-116,219-232`), and steals the
active bearer `auth_token`. It then owns the post-pairing Noise content channel
forever. The app pins nothing out of band (standard rustls CA roots; the gateway
static is learned *inside* the SPAKE2 channel — `app/mobile/core/src/pairing.rs:186`),
so nothing downstream catches it.

The only residual barrier — the 6-digit confirm code — **does not stop a
determined relay.** `derive_confirm_code(K)` is ~20 bits, folds only `K` (not the
exchanged statics), and has **no commitment phase** (`kdf.rs:30,63-76`). Because C
is the last mover on the app-facing leg, it can grind its own SPAKE2 ephemeral
(~2²⁰ targeted, or ~10³/leg birthday — seconds) until
`confirm(K_app) == confirm(K_gw)`; both screens then show the same number. The
grind is pressure-free: the app sets **no receive timeout** waiting for the reply
(`app/mobile/src-tauri/src/pairing.rs`), and the gateway re-hosts every ~500 ms–2 s
(`crates/gateway/src/channel/relay_pair.rs` `REHOST_BACKOFF`/`HOST_POLL_INTERVAL`).
The operator confirm prompt also **defaults to yes** (`crates/cli/src/commands/device.rs:214`).

Severity: **high**. The default pairing path is the public proxy
`wss://proxy.baybo.space` with a shared `guest` admission key
(`crates/cli/src/commands/device.rs:36,129-148`), so "the relay is hostile" is the
*default* trust position, not a corner case.

## Fix — split the code, drop the PAKE, authenticate with Noise XXpsk0

Two values instead of one:

- **`rendezvous_id`** — a public **UUIDv4** (122-bit). The *only* thing the relay
  ever sees. It is the broker key, the `/pair/{host,join}/{id}` URL param, the
  slot lookup key, and the first-frame selector. It is not secret and routes
  nothing else.
- **`secret`** — a **CSPRNG, 256-bit** value carried *only* in the QR (out of
  band), never transmitted over the relay, never in a URL, never logged, never
  persisted past the slot. Used as the Noise **PSK**.

Pairing then runs **`Noise_XXpsk0_25519_ChaChaPoly_SHA256`** instead of SPAKE2.
The relay sees only the rendezvous id and opaque Noise frames; lacking the PSK it
cannot complete a handshake with either side, so it is reduced from MITM to
denial-of-service.

### Why XXpsk0 (not NN, not the literal "NK")

`XX` is the pattern whose handshake **exchanges both statics in-band** — exactly
today's model (the gateway static is not known to the app ahead of time; the app
static is not known to the gateway). `XXpsk0` adds the QR secret as a PSK mixed in
**before the first ephemeral**, which *authenticates* that otherwise-anonymous
static exchange. The result: a MITM that lacks the PSK cannot complete the
handshake, and the handshake hash `h` **binds both statics by construction** — so
the confirm code derived from `h` (below) attests the real device identities, with
nothing extra to fold in.

- **NN** (the minimal drop-in) would also work — PSK authenticates, statics ride
  as post-handshake transport payloads as today — but `h` would *not* cover the
  statics, forcing `HKDF(h ‖ app_static ‖ gw_static)` for the SAS and leaving NN's
  symmetric reflection risk to be patched via role labels. XX is cleaner.
- **NK** (what the original sketch said) requires the app to know the gateway's
  static **before** the handshake → the gateway static would have to ship in the
  QR and the app would persist it from there instead of learning it in-band. That
  is a *different, larger* model change. We are **not** doing NK. (NKpsk0 remains a
  valid future "pin the gateway out-of-band" upgrade if ever wanted; out of scope
  here.)

### THE load-bearing invariant: the secret must be high-entropy

A Noise PSK is **not** a PAKE. Against the relay — an *active* participant — one
captured transcript is enough to mount an **offline dictionary attack** on the PSK
(try candidate → recompute chaining key → check the AEAD tag). SPAKE2's "one
online guess, no offline attack" property, which is what made a short human code
safe, **does not survive the switch**. Therefore:

- The secret is CSPRNG-generated, **≥128 bits — use 256 bits / 32 bytes** (snow's
  PSK width, so no expansion needed).
- It travels **only** in the QR. There is **no typeable form** of it (see the
  fallback change below). A short / typeable secret would be *worse* than today —
  offline-cracked instead of online-one-guess.

This single requirement is the spine of the whole fix; everything else is
plumbing. Encode it as a code-level invariant (32-byte type, no string
constructor) with a comment pointing here.

## Proposed handshake

Roles: **app = initiator, gateway = responder** (preserves "app speaks first").
`XXpsk0`:

```
        app (P, initiator)                         gateway (A, responder)
msg1 ─► PairFrame::Hello { rendezvous_id, e }      claim slot by rendezvous_id,
        (psk, e)                                   load secret from vault,
                                                   build XXpsk0 responder
msg2 ◄─ PairFrame::HandshakeReply { msg }          (e, ee, s, es) — gw static
        app now holds gw static (in h)
msg3 ─► PairFrame::HandshakeFinal { msg }          (s, se) — app static + the
        payload = DeviceHello fields               DeviceHello payload
        ── both sides in transport mode, share final h ──
        compute confirm_code = HKDF(h)             compute confirm_code = HKDF(h)
        show on phone; user taps accept            publish to slot; operator `y`
msg4 ─► PairFrame::Sealed(DeviceConfirm)           (transport msg)
msg5 ◄─ PairFrame::Sealed(GatewayWelcome)          (transport msg, after operator
                                                   confirm; carries auth_token)
```

- `Hello` carries `rendezvous_id` so the gateway can claim the in-flight slot by
  it (and it is bound into the prologue); on the relay route it is also the path
  param and MUST match.
- `DeviceHello`'s non-static fields (`device_id`, `label`, `apns_token`,
  `apns_env`) ride as the **msg3 payload** (authenticated by the app static). The
  app static itself is now an XX handshake token, not a payload field.
- `DeviceConfirm` / `GatewayWelcome` are **transport-mode** messages (snow's
  implicit nonce — drop the explicit `nonce` field from `PairFrame::Sealed`).
- Optional nicety: the app can compute msg3 (hence `h` and the confirm code)
  **without transmitting it**, show the code, and only send msg3 + `DeviceConfirm`
  once the user accepts — so the app static is never revealed on a declined pair.

### Prologue binding (do not skip)

Both ends construct an identical, **canonical, versioned, length-prefixed**
prologue and pass it to the snow `Builder`:

```
prologue = v1 ‖ rendezvous_id ‖ endpoint ‖ role-labels
```

It is authenticated (folded into `h`) but **not secret** — keep the PSK out of it.
A one-byte disagreement aborts the handshake. This buys, for free:

- **anti-splice / anti-cross-binding** — the relay can't wire app-of-pairing-X to
  gateway-of-pairing-Y (mismatched rendezvous id), nor cross-bind two relays
  (mismatched `endpoint`), even if a secret were ever reused;
- **role binding** — replaces SPAKE2's `ID_APP`/`ID_GATEWAY` labels
  (`pake.rs:19-21`); blocks reflection / role confusion;
- **downgrade resistance** — the relay can't swap the bound `endpoint`; the app
  refuses if `s` is absent.

### Confirm code = channel binding

Derive the 6-digit confirm code from the **Noise handshake hash**
(`HandshakeState::get_handshake_hash()`), not from the secret/K. `h` commits to the
whole transcript — prologue, both ephemerals, the PSK mix, **and both statics**
(XX) — so a matching code proves a single, live, untampered session and is **not
grindable or precomputable**. Reuse the `kdf.rs` `CONFIRM_CODE_INFO` /
`derive_confirm_code` shape, fed `h` instead of `K`. Keep the two-sided human
confirm, but reposition it as **authorization** (operator decides "pair this
device") and the backstop if the QR secret leaks (shoulder-surf / forward) — and
flip the operator prompt default to **no** (`device.rs:214`).

## Implementation stages

### Stage 1 — crypto core (`crates/device-proto`)

- Delete `pake.rs` (and the `spake2` dep from `Cargo.toml` + workspace).
- New `psk_pair.rs`: an XXpsk0 state machine over `snow` (reuse the cipher suite,
  `KEY_LEN`, and the chunking codec already in `noise.rs:107-199`). Expose
  `start_app` / `start_gateway` (or initiator/responder) taking the 32-byte PSK +
  the prologue bytes; expose `handshake_hash()` post-handshake.
- `kdf.rs`: add `derive_confirm_code_from_h(h: &[u8])`; the `push_key` / any
  long-lived subkey now derives from a Noise output (e.g. a transport-derived
  secret or `h`-based HKDF), not the SPAKE2 `K`. Keep `PUSH_KEY_INFO` label.
- `pairing.rs`: rework `PairFrame` to `Hello { rendezvous_id, e }` /
  `HandshakeReply { msg }` / `HandshakeFinal { msg }` / `Sealed { msg }` /
  `Reject`. Drop the `code` field everywhere; `DeviceHello` loses `static_pubkey`
  (now an XX token) — or keep it carried as the msg3 payload, but never as the PSK.
- Add a 32-byte `PairingSecret` newtype (no `From<String>`; CSPRNG ctor only) and a
  prologue builder `const` + helper.

### Stage 2 — slot / wire / QR model + **secret hygiene**

Split `DevicePairingSlot.code` into `rendezvous_id` (public) + `secret`
(credential). The secret is **vaulted, not a plaintext column**:

- `crates/store/src/device_pairing.rs` — slot row: `rendezvous_id` replaces
  `code` as the key; **do not** add a plaintext `secret` column. Store the secret
  in `SecretVault` keyed by `rendezvous_id` (encrypted, TTL'd, zeroized on
  consume). `list_slots()` must never return it.
- `crates/pairing/src/device_service.rs` — `mint()` returns
  `(rendezvous_id, secret)`; `complete()` writes **`rendezvous_id` only** into the
  durable `DeviceRow` (today it stores `pairing_code: Some(slot.code)` —
  `device_service.rs:134`, the never-deleted row). `code.rs` 6-char minting is no
  longer used for device pairing (it stays for the channel-pairing gate).
- `crates/gateway/src/channel/device_pair.rs` / `relay_pair.rs` — `drive()` loads
  the secret from the vault by rendezvous id, runs the XXpsk0 handshake, computes
  the confirm code from `h`. `GatewayWelcome.pairing_code` → `rendezvous_id`
  (public audit handle only).
- `app/mobile/core/src/pairing.rs` + `src-tauri/src/pairing.rs` — `PairedGateway` /
  `PairedSummary` / `PairedRecord` carry `rendezvous_id`, never the secret;
  `PairingRequest` takes the 32-byte secret + rendezvous id parsed from the QR.
- QR payload: `baybo://pair?h={endpoint}&r={rendezvous_id}&s={secret}&k={key}`.
  `s` is bearer secret material: stderr only (rendered straight to a QR), never
  `device list`, never structured JSON, never logs.
- `crates/cli/src/commands/device.rs` — `device list` shows `rendezvous_id`, never
  the secret; the `CODE` column / JSON `pairing_code` field becomes `rendezvous_id`.

### Stage 3 — relay routing rename (`remote-host`)

Pure rename, no behaviour change: `PAIR_HOST`/`PAIR_JOIN` params `{code}` →
`{rendezvous_id}` (`remote-host/crates/protocol/src/relay.rs:13-14` + builders),
`serve.rs` handlers, and the relay's doc comments (`serve.rs:16-17`,
`broker.rs:11-13`) which currently claim "C never sees the code" — restate the
honest model (C sees only the public rendezvous id).

### Stage 4 — hardening

- **Leg-stealing DoS** (survives the fix and is mildly worse — the rendezvous id is
  public). Anyone who learns the id (the relay, a QR photographer, or any holder of
  the shared `guest` key) can `/pair/join/{id}` → `try_match` steals the gateway's
  parked host leg → fails the PSK handshake → griefs pairing. Mitigate: **re-park
  the host leg immediately** on PSK-auth failure (don't tear down + backoff),
  rate-limit `/pair/join` per rendezvous id, and emit a metric/alert on repeated
  PSK failures. Availability only (a hostile relay can always DoS), but the shared
  key makes it reachable by non-relay parties.
- **Scope / drop the shared `guest` key** — a shared admitted key lets any guest
  grief any other guest's rendezvous.
- **Remove the typeable fallback** (`device.rs:~157` prints `code:` when the QR
  can't render). A 256-bit secret isn't typeable, and a low-entropy one is
  offline-crackable. As built: on render failure the command **errors out** asking
  the operator to widen the terminal and retry — it never prints a shortened
  secret **and never writes the payload to disk** (the secret must not persist).
- **App receive timeout** on the handshake reply (today there is none), so a relay
  can't pin the app open.

## Design constraints

- **Fail closed.** No PSK / wrong PSK / mismatched prologue → handshake aborts, no
  token. An absent `s=` in the QR → the app refuses to pair (no silent downgrade).
- **The secret never leaves the QR / the vault.** Not on the wire, not in a URL,
  not in a log, not in the durable `DeviceRow`, `device list`, `GatewayWelcome`,
  `PairedSummary` (a ts-rs DTO returned to the webview), or any plaintext slot
  column. Only `rendezvous_id` (public) goes to those places. This **flips** the
  channel-pairing doc's "the code is operator-facing, not secret" assumption —
  device pairing now has *two* fields with opposite handling.
- **Fresh + single-use.** A new secret + rendezvous id per pairing; consumed on
  `complete`, zeroized; never reused. XXpsk0's `ee` gives forward secrecy even if
  the secret later leaks; a single-use secret keeps the leak window a single
  rendezvous.
- **Relay-only.** Pairing runs exclusively over the relay; there is no direct/LAN
  pairing route. The prologue binds `endpoint` so the relay can't cross-bind the
  app to a gateway on a different relay.
- **Relay admission (`instance_key`) is orthogonal.** It is the relay's *own*
  anti-abuse allow-list (enforced by C), not protection against a hostile C — keep
  it, but the PSK is what defends the app↔gateway channel. Do not conflate.
- **Observability.** Log the hashed `rendezvous_id` at most (mirror the existing
  `short_hash` of device/user in `device_pair.rs`); never the secret, never the
  confirm code.

## Out of scope (separate calls)

- ~~**One-app-per-gateway**~~ — **now done** (a policy gate on the device count,
  orthogonal to the crypto here). The binding is 1:1: one gateway = one user =
  one app, replace-with-confirm. See
  [`remaining.md` §7](remaining.md#7-one-gateway--one-app-11-binding--done). The
  crypto scheme is unchanged (each device still has its own static + push key);
  the policy just caps the live count at one per user.
- **NKpsk0 / gateway-static-in-QR**: a possible future out-of-band gateway pin;
  not needed to close the MITM.

## Related

- `crates/device-proto/src/pake.rs:1-12,32-50` — SPAKE2 to delete; the false
  "C never learns the code" claim.
- `crates/device-proto/src/kdf.rs:30,63-76` — confirm code; re-derive from `h`.
- `crates/device-proto/src/pairing.rs` — `PairFrame` + sealed message reshape.
- `crates/device-proto/src/noise.rs:107-199` — chunking codec to reuse for the
  XXpsk0 transport.
- `crates/pairing/src/device_service.rs:48-141` — `mint`/`complete`; split the
  code, vault the secret, store only `rendezvous_id` in the row.
- `crates/gateway/src/channel/device_pair.rs:90-240` — `drive()`; the host
  handshake.
- `crates/gateway/src/channel/relay_pair.rs` — relay host manager + re-park fix.
- `remote-host/crates/relay/src/serve.rs:16-17,154-207`,
  `remote-host/crates/relay/src/broker.rs:11-13,70,124-129` — routing rename + DoS
  re-park; honest doc comments.
- `remote-host/crates/protocol/src/relay.rs:13-14,37-58` — `{code}` → `{rendezvous_id}`.
- `crates/cli/src/commands/device.rs:36,129-157,210-217` — QR payload, removed
  typeable fallback, operator-confirm default → no.
- `app/mobile/core/src/pairing.rs`, `app/mobile/src-tauri/src/pairing.rs` — app
  state machine, QR parse, persisted record; add the handshake-reply timeout.
- [`docs/modules/pairing.md`](../../modules/pairing.md) — the *channel*-pairing gate
  (separate); update its cross-reference if it grows a device-pairing section.
