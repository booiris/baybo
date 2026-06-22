# mobile-remote-host — iOS Companion, Device Pairing, Blind Relay & APNs Push

> **Status: proposed / design-stage.** None of the crates, services, tables,
> endpoints, or iOS targets described here exist yet. This is the architecture
> reference (the repo's design-doc-first convention). The **phase-1 execution
> plan** is [`todo/mobile/phase1.md`](todo/mobile/phase1.md) (the full phase
> roadmap is [`todo/mobile/README.md`](todo/mobile/README.md)); when the two differ,
> the resolved decisions in *this* doc plus that plan win. Treat every `crates/…`
> / `aura-remote-host/…` / `app/mobile/ios/…` reference as a *target*, not a
> *fact*, until the corresponding code lands.

## Problem

We want a native-feeling iOS companion app (built with **Tauri**) for Aura that:

1. **Pairs by scanning a QR code** instead of pasting an admin token.
2. **Receives a push notification when a turn completes** (the agent finished
   replying) even while the app is backgrounded or killed, showing a **real
   decrypted lock-screen preview**.
3. **Opens to the full thread** by self-pulling it over an end-to-end channel.

Two hard external walls shape the whole design:

- **APNs is bound to the app's Apple Developer Team.** A push is only accepted by
  APNs if it is signed with the `.p8` key of the team that owns the app's bundle
  ID (`apns-topic`), with the JWT `iss` = that 10-char Team ID. A cross-team
  request is rejected (`403 TopicDisallowed`). There is **no Apple-blessed
  delegated push.** So a third party who self-hosts an Aura gateway **cannot**
  talk to APNs for *our* published app — they don't have, and must never be
  given, our `.p8`.

- **A self-hosted gateway is usually not publicly reachable** (binds
  `127.0.0.1:8888` by default — see `crates/config/src/gateway.rs`; sits behind
  NAT). APNs wakes the phone from anywhere, but the phone then has to reach *that
  operator's* gateway to fetch the actual content.

The product constraint is load-bearing: **anyone deploying Aura must be able to
get notifications**, without their own Apple Developer account, their own app, or
a public IP. That forces a small amount of **operator-run shared infrastructure**
(run by us, the app publisher), consolidated into **one service**:

- **`aura-remote-host`** — holds the `.p8` and relays gateway-encrypted preview
  blobs to APNs (the **push** role, *mandatory*); and brokers pairing +
  connectivity for NAT'd gateways (the **relay + rendezvous** role, *operated;
  content prefers direct*). It also carries **admission** (which gateways may use
  it) and an operator **dashboard**.

`aura-remote-host` carries other people's private AI conversations on the wire, so
it is designed to be **blind**: end-to-end encrypted between the phone and the
operator's own gateway, so we (and Apple) only ever see ciphertext + routing
metadata, never plaintext.

> **Naming.** This doc uses three fixed names to avoid the "gateway" collision:
> **A** = the user's *aura gateway* (runs the agent, holds the transcript);
> **C** = *`aura-remote-host`* (the operator-run shared infra); **P** = the iOS
> *app* + its Notification Service Extension. The two E2E ends are **P and A**.

## Architecture overview

```
                                  ┌──────────────────── operator-run (us): aura-remote-host (C) ───────────────────┐
 ┌─────────────┐                  │                                                                                  │
 │  iOS app (P) │  QR pair (SPAKE2)│   ┌──────────────┐   ┌────────────┐   ┌───────────┐   ┌───────────────┐         │
 │  Tauri +     │◄──── rendezvous ─┼──►│  relay +      │   │ admission  │   │  push      │   │  dashboard     │        │
 │  NSE         │                  │   │  rendezvous   │   │ (per-inst  │   │ holds .p8  │   │ (ops metadata) │        │
 │  Keychain /  │  content fetch   │   │ (blind pipe,  │   │  keys)     │   │ ES256 JWT  │   └───────────────┘         │
 │  CryptoKit / │  direct-first ───┼──►│  NAT coord)   │   └────────────┘   └─────┬─────┘                            │
 │  Rust core   │  (E2E, else relay)│  └──────▲────────┘                         │                                  │
 └──────┬───────┘                  └─────────┼────────────────────────────────────┼──────────────────────────────────┘
        │ encrypted preview                  │ Noise frames (relay blind)         │ POST /notify (per UserChat turn)
        │  (blind blob)                      │ + control conn (A dials out)       │
        ▼                            ┌───────┴──────────┐                         │
   ┌─────────┐  APNs (us → Apple)    │  user's aura      │─────────────────────────┘
   │  Apple   │◄─────────────────────│  gateway (A)      │  turn complete (JobLifecycle::Completed, UserChat)
   │  APNs    │  push → api.push.apple│  + device registry│  → encrypt preview → POST to C's push
   └─────────┘                       │  + Noise endpoint  │
        │  delivers push to device   └────────────────────┘
        └──────────────────────────► iOS app (NSE decrypts preview; then app self-pulls full content over E2E)
```

### Trust model

- **The two "ends" are P and A.** Everything in between — C's relay, C's push,
  Apple — must only see ciphertext + routing metadata, never plaintext.
- **A is a trusted end and sees plaintext** — it runs the agent and already stores
  the transcript in its own libsql. "E2E" here protects content from *us* (the
  shared-infra operator) and Apple, **not** from the user's own gateway. Do not
  over-read "E2E" as hiding content from A.
- The phone holds the only copy of the keys needed to read pushed content; C's
  push role holds only `device_id → APNs token`, never content.

### `aura-remote-host` (C) — one service, isolatable `.p8`

C is **one Cargo workspace** of normal host services, but its components have very
different secrets, exposure, and scaling, so they stay separable:

| Component | Role | On the path when | Cost / exposure |
|---|---|---|---|
| **push** | Holds the `.p8`; relays a **gateway-encrypted preview blob** to APNs for any Aura instance (blind) | Every UserChat turn completion | Cheap (small JSON POSTs, no persistent connection). **Holds the crown-jewel key → built as its own binary.** |
| **relay + rendezvous** | Blind byte-pipe for NAT'd gateways + SPAKE2 pairing rendezvous + NAT-traversal coordination | Pairing always; content **only when direct fails** | Idle connections cheap & bounded; **content bandwidth is open-ended** (relays streaming) — content **prefers direct**. High-exposure, multi-tenant, DoS-attractive. |
| **admission** | Per-instance keys (the Bitwarden installation-id model); rate limits | Every push + relay op | "Auth" = **machine-to-machine instance admission only**; never device auth, never plaintext. |
| **dashboard** | Operator ops UI: registered instances, hashed bindings, rate/usage, APNs token health, relay stats | — | **Metadata only.** Blind; shows no conversation content. |

**One folder ≠ one blast radius.** Dev can build a single binary; production keeps
the `.p8`-holding **push** independently deployable so the high-exposure **relay**
surface can't reach the key. The push path needs **no persistent connection** (A
makes an outbound HTTPS POST on turn completion — the Matrix→Sygnal /
Bitwarden→push.bitwarden.com shape). The relay path **does** require A to hold a
persistent **outbound control connection** to C (so the phone can reach a NAT'd A).

### Why a central push service is mandatory

This is the question the whole design hinges on, so it is spelled out.

- A `.p8` APNs auth key is **team-scoped, but the team must own the app.** The
  provider JWT's `iss` is the Team ID; `apns-topic` is the bundle ID; APNs
  validates the topic belongs to the issuing team. A self-hoster's own key (under
  *their* team) cannot target an app whose bundle ID lives under *our* team.
- The `aps-environment` entitlement + bundle ID are **baked into the signed
  build**; device tokens are unique to the (device, app) pair and only routable
  by a provider authenticating as that app's team.
- **No delegated/third-party push exists.**

⇒ For many independent self-hosted gateways to deliver push to **one** shared
published app, each must hand its push to a service that **does** hold our `.p8`.
This is exactly how the ecosystem solves it:

| Project | Relay service | Holds the key | Relay sees content? |
|---|---|---|---|
| Mastodon | `webpush-apn-relay` | app vendor | No — Web Push E2E, relay forwards ciphertext |
| Bitwarden / Vaultwarden | `push.bitwarden.com` | Bitwarden | No — content-less wake-up |
| ntfy (self-hosted) | `upstream-base-url → ntfy.sh` | ntfy.sh | No — only message-id + topic hash |
| Matrix | Sygnal push gateway | app vendor | No — `event_id_only`, client fetches+decrypts |
| Home Assistant (official apps) | `mobile-apps.home-assistant.io` | HA project | **Yes — plaintext** (the anti-pattern to avoid) |

The only way to *avoid* the central service is **one app per operator** (separate
$99/yr, separate App Store listing + review, users install a different app per
server). Rejected — it defeats "anyone can get notifications."

## Design

### Device registry (in the aura gateway A)

Persistent per-device identity, distinct from the in-memory `ChannelTokenTable`
(`crates/gateway/src/auth/token.rs`) — device credentials must survive a gateway
restart, so they go in libsql.

```
devices                         -- new libsql table (one per gateway A)
device_id        TEXT NOT NULL  -- client-generated, PER PAIRING (multi-gateway: see below)
user_id          TEXT NOT NULL  -- owning principal
label            TEXT NOT NULL  -- "Booiris iPhone"
device_pubkey    BLOB NOT NULL  -- X25519 static public key (from pairing)
auth_token       TEXT NOT NULL  -- 256-bit hex bearer for REST/WS (persisted)
status           TEXT NOT NULL  -- 'pending' | 'approved' | 'revoked'
pairing_code     TEXT           -- the retained SPAKE2 code C, used as the approval handle
created_at       INTEGER NOT NULL
approved_at      INTEGER
last_seen_at     INTEGER
PRIMARY KEY (user_id, device_id)
UNIQUE INDEX idx_devices_auth_token ON devices(auth_token)
```

- `DeviceStore` trait + row type in `aura-store`; `LibsqlDeviceStore` in
  `aura-storage` — same split as `ChannelPairingStore` (see
  [storage.md](modules/storage.md), [pairing.md](modules/pairing.md)).
- The channel auth middleware (`crates/gateway/src/auth/channel.rs`) gains an
  `AuthedClient::Device { user_id, device_id }` variant resolved by `auth_token`
  lookup. It authorizes a **scoped subset**: the `/v1/chat/*` endpoints +
  `/v1/channel-ws` (register as `ChannelType::ios`, a `Subscribed` channel like
  web `http`). It must **not** unlock the full admin surface.
- **APNs device tokens do not live here.** They live in C's push store. A only
  needs `device_id` to address a push.
- **Session rows are never deleted** (project rule). Revoking a device flips
  `status='revoked'` and invalidates its `auth_token` — **the row stays** (keeps
  the audit trail and stops the `auth_token` UNIQUE slot from being silently
  reused). It never touches the session, the transcript, or the channel binding. A
  hard delete, if ever wanted, is a separate explicitly-operator-triggered command,
  not the default revoke. Dead APNs tokens are unbound, not deleted.

#### Multi-gateway (one app install ↔ N gateways)

One install may pair with several gateways (home, work). Resolved model:

- `device_id` is **per-binding unique** (not per-install): each pairing lands a
  distinct `(user_id, device_id)` row in *that* gateway's `devices` table, with
  its own push key `K_i`.
- The push payload carries **`bid`** (= that binding's `device_id`) so the NSE
  selects the right push key from the shared Keychain. `collapse_id = bid:session_id`
  so pushes from different gateways never coalesce.
- **APNs token registration is gateway-mediated:** P sends `{ apns_token, env,
  device_id }` to A over Noise; **A** registers `device_id → apns_token` with C's
  push using A's `instance_key`. P never holds a C credential.
- **Accepted metadata leak:** one install has one APNs token, so C can correlate a
  phone's multiple bindings via the shared token. C still sees no content;
  inherent to multi-gateway.

### QR pairing (SPAKE2 over rendezvous)

Orthogonal to the existing **channel** pairing ([pairing.md](modules/pairing.md));
this is **device** pairing: it binds a *device* to a *gateway* and bootstraps an
E2E key. Pairing crosses the untrusted C, so a balanced PAKE over a short code is
the right tool.

QR payload (rendered in the web dashboard / `aura device pair`):

```json
{ "rendezvous": "wss://remote.aura.example/r", "code": "WORMHOLE-7-foo-bar" }
```

```
   iOS app (P)                 rendezvous (C, blind)                  gateway (A)
     │  scan QR (code C)                 │                                │
     │── SPAKE2 msg(from C) ────────────►│──────── SPAKE2 msg(from C) ───►│
     │◄───────── SPAKE2 msg ─────────────│◄──────────── SPAKE2 msg ───────│
     │   both derive same strong key K locally; C only relays opaque blobs  │
     │===== K-encrypted, authenticated channel ===========================│
     │  exchange long-term X25519 static pubkeys (TOFU via K; C can't read) │
     │  A also hands P its relay_node_id + direct-reachability candidates  │
     │  derive HKDF push key (shared app↔gateway)                          │
     │  A writes a PENDING device row (auth_token inert) + retains C       │
     │  P sends {apns_token, env, device_id}; A registers it with C's push │
     │                                                                     │
  operator: `aura device approve <code>` → row → approved, token activates │
```

- **SPAKE2** (balanced PAKE, magic-wormhole model; the resolved choice): C
  forwards only opaque PAKE blobs — it never learns `C` or `K`, and a short `C` is
  safe because a PAKE allows at most one online guess per run and no offline
  dictionary attack. Rust: the `spake2` crate (verify maintenance at integration
  time; the PAKE is an internal, swappable detail of `aura-device-proto`).
- `C`'s job is to authenticate a **one-time** channel over which the two sides
  swap **long-term static public keys** and derive the symmetric **push key**.
  A's static pubkey is exchanged **inside** the K channel — authenticated by the
  SPAKE2-derived K that C **cannot read or forge** (C relays opaque blobs only),
  **not** gated by C — and is not in the QR. After derivation `C`'s crypto role is
  done; it is **retained on the pending device row** purely as the human
  **approval handle**.
- **Residual pairing risk (stated honestly):** a PAKE permits exactly **one online
  guess of the short code per run**. A C that guesses the code on that one try
  could complete SPAKE2 and substitute its own static key — after which Noise would
  authenticate the device to C permanently. Mitigate with: per-code rate-limit /
  lockout of pairing attempts, the single-use code + short TTL, and the explicit
  operator `aura device approve` as a second human gate before the `auth_token`
  activates. Do **not** rely on Noise alone to exclude an operator MITM — that
  holds only if pairing itself was not MITM'd.
- **Transport.** A dedicated **token-free WS route `/v1/device/pair`** on A
  carries the PAKE blobs + the K-channel exchange (gated by the PAKE itself,
  since pairing happens before any token). The `aura-device-proto` SPAKE2 logic is
  transport-agnostic, so the same blobs ride the rendezvous now and any future
  path unchanged.
- **Pending state.** A libsql `DevicePairingStore` mirrors `ChannelPairingStore`:
  reuse `crates/pairing/src/code.rs` `generate_unique`, the TTL, and the janitor
  sweep. A new `DevicePairingService` drives the SPAKE2 + key-exchange flow.
- **Explicit operator approval** (resolved): pairing creates a `pending` row whose
  `auth_token` is **inert until** `aura device approve <code>` (mirrors `aura pair
  approve`). The SPAKE2 handshake + key exchange may complete while still pending
  — **approval gates *access*, not the handshake.**

### E2E session (Noise)

After pairing, each side holds the other's X25519 static public key. Every
connection (direct or relayed) runs a **Noise** handshake:

- `IK` for reconnects (initiator already knows responder's static key — near
  zero-RTT); `XX` for first contact; `XXfallback` if a cached key is stale.
- Authenticated X25519 DH → fresh ephemeral session keys (forward secrecy) → AEAD
  frames (ChaCha20-Poly1305). Rust: `snow`.
- C holds **no static private key**, so — **once the static keys were authentically
  exchanged at pairing** (see the residual-risk note above) — Noise's static-key
  authentication blocks an active MITM on every later session, including by the
  operator.

Layering — TLS is hop-by-hop, Noise is end-to-end:

```
app ══TLS══ C(relay) ══TLS══ gateway     TLS terminates at C (C can read TLS)
 └──────── Noise (E2E) ────────┘          …but TLS wraps Noise ciphertext → C learns nothing
```

**Shared Rust core.** SPAKE2 + Noise + the AEAD preview framing + the
device-pairing wire live in `crates/aura-device-proto`; the MessagePack `Frame`
codec lives in `crates/aura-wire` (extracted from `aura-channels`). **A** and the
app's **`aura-mobile-core`** depend on the same crates, so both run the *same*
wire protocol + crypto — interop is guaranteed by construction.

> **No UniFFI.** Because the app is **Tauri**, the Rust core runs in `src-tauri`
> (reached from the webview via Tauri commands) and the NSE decrypts with native
> **CryptoKit** — *no Swift consumes the Rust core*. So there is no `xcframework`
> and no UniFFI layer; `aura-mobile-core` is a plain Cargo dependency of
> `src-tauri`. (The original native-Swift design assumed UniFFI; Tauri removes it.)

### Blind relay + rendezvous + connectivity (in C)

Solves "phone can't reach the NAT'd gateway" and hosts pairing. **Resolved
connectivity model:** P and A **both connect to C**; C coordinates; if the two can
talk directly they go **P2P-direct** (C's data plane off the path); otherwise C
**blind-relays** (the Tailscale-DERP / iroh shape).

- **A holds a persistent outbound control connection to C** when relay is enabled
  (a first for the gateway, which has only ever been a server; `tokio-tungstenite`
  is already a workspace dep). Data legs open on demand: when P requests A's
  `relay_node_id`, C signals A over its control connection to open a data leg, and
  C pipes the two legs blind.
- **The relay is a pure blind byte-pipe.** `Frame` runs E2E inside Noise; C never
  sees it. One app connection = one Noise tunnel; session multiplexing is the
  `Frame::Subscribe` layer's job, not the relay's.
- **Routing** uses a **C-assigned, non-secret `relay_node_id`** handed to P at
  pairing (over K). P never holds A's `instance_key`. Noise authenticates the real
  endpoints, so a non-secret routing id is safe. Pairing-rendezvous and
  content-relay share **one** C primitive ("match two legs, copy bytes blind"),
  keyed by the pairing code vs. `relay_node_id`.
- **Content prefers direct.** Before relaying, P tries A's **direct-reachability
  candidates** (LAN / the user's Tailscale / the user's reverse proxy — advertised
  by A at pairing over K) and falls back to C only when direct fails. This keeps
  the dominant "phone + server on the same network" case off C entirely. A must
  expose a **non-loopback Noise endpoint** for this (safe: Noise authenticates
  E2E).
- **Resource profile.** Idle control connections are cheap and bounded (≈tens of
  KB each; Rust/tokio scales to 10⁵–10⁶ connections). The **open-ended** cost is
  **content bandwidth**, which is 100 % on C in always-relay and scales linearly
  with usage — which is exactly why direct-first (now) and NAT hole-punching
  (deferred) exist. Budget for the fallback traffic regardless: it is an
  open-ended, multi-tenant bandwidth + abuse commitment.
- **Deferred:** full **NAT hole-punching** and adopting **iroh** (DERP-like relay
  + DCUtR-like hole punch in one Rust lib) — a later optimization to take content
  bandwidth off C without configured direct reachability. Phase-1 E2E is **our
  own Noise**, not iroh's node encryption, to keep the trust model clean.
- **Admission** (so it isn't open infra): each Aura instance registers and presents
  a per-instance key (Bitwarden installation-id+key model). Rate-limit per
  instance. The relay URL is **configurable** (`relay.base_url`), defaulting to
  our public instance but overridable to a self-hosted host.

Prior art for a blind byte-pipe: Tailscale **DERP**, **iroh-relay** (Rust).
Cloudflare Tunnel / ngrok use the same outbound-dial trick but **TLS-terminate at
the edge** (provider can read plaintext) — so they are *not* a model for the blind
property; the Noise layer is what makes our relay blind regardless.

### Push (in C) — the notification component

The only component that holds the `.p8`. Built as its own binary.

- **APNs sender.** Token-based auth: ES256 JWT signed with the `.p8` (Key ID +
  Team ID), `authorization: bearer <jwt>`, POST `https://api.push.apple.com/3/
  device/{token}` (prod) / `api.sandbox.push.apple.com` (dev). Reuse one JWT and
  refresh every ~30–50 min. Headers: `apns-topic` = bundle id, `apns-push-type:
  alert`, `apns-priority: 10`, `apns-collapse-id` = `bid:session_id`. Rust: `a2`
  crate, or a thin `reqwest`(http2) + `jsonwebtoken`(ES256) (verify `a2`
  maintenance at integration time).
- **Store.** `device_id → { apns_token, apns_env (sandbox|production) }` +
  per-instance admission keys. APNs env tracking per device is the #1 footgun:
  dev-signed builds only work against sandbox, TestFlight/App Store against
  production; the same `.p8` serves both — only host differs.
- **Ingest.** `POST /notify { instance_key, device_id, collapse_id, kid, enc, n }`
  from an aura gateway, where `enc`/`n` are the **opaque AEAD ciphertext + nonce
  of the lock-screen preview**, already encrypted by A with the device's push key,
  and `kid` is the key epoch. push validates `instance_key`, looks up the APNs
  token, builds the APNs payload (`aps.alert` = generic placeholder,
  `mutable-content: 1`, `apns-push-type: alert`, `apns-priority: 10`, with
  `enc`/`n`/`kid`/`bid` copied verbatim into custom top-level keys), signs the
  JWT, and forwards. **It never decrypts `enc` — it stays blind.** Rate-limit per
  instance + per device.
- **Token pruning.** On `400 BadDeviceToken` / `410 Unregistered`, unbind the
  device's APNs token (honor the `410` timestamp before deleting). Never delete the
  aura session.
- **Self-hostable.** `push.gateway_url` is configurable, defaulting to our public
  instance.

### Turn-completion hook (in the aura gateway A)

The push *trigger* reuses the existing turn-completion signal. Turn completion is
`JobLifecycle::complete()` (`crates/job/src/lifecycle.rs`) flipping a turn-kind job
to `Completed` and publishing `JobLifecycleEvent` on its broadcast bus — the same
bus the turn-state projector (`spawn_turn_state_projector`,
`crates/agent/src/actor/supervisor.rs`) already subscribes to.

`JobLifecycleEvent` carries `{ job_id, session_id, parent_job_id, phase }` only.
To let the dispatcher filter without re-fetching the Job, **add `kind:
JobInputKind` and `shape: JobShape`** to the event (additive; the projector
ignores them).

A new **push dispatcher** task (`crates/gateway/src/push/`) subscribes and, for
each terminal edge:

1. **Filter to real user turns: `shape == Turn && kind == UserChat`.** This
   excludes `Cron`, `System` (background compression), `Spawned`,
   `SubagentNotification`, **and** `/compact` — which is a `UserChat`-*input* but
   `Maintenance`-*shape* job (`crates/job/src/kind.rs` documents this; filtering on
   `kind` alone would wrongly buzz the phone on every `/compact`, with no new
   assistant message). Note `JobInputKind` is `{ UserChat, Cron, System, Spawned,
   SubagentNotification }` — there is **no** `Compression` variant.
2. `session_id → user_id`; look up **`approved`** `devices` for that `user_id`.
3. Fetch the session's last assistant message; build a **short preview** (title +
   first ~200 chars, generic fallback for non-text, bounded so the ciphertext fits
   the APNs 4 KB payload); **per-device AEAD-encrypt** it with that device's **push
   key**; `POST { instance_key, device_id (bid), collapse_id: bid:session_id, kid,
   enc, n }` to `push.gateway_url`. The full reply is **not** sent — the app
   self-pulls it on open.

The gateway holds each device's symmetric **push key** (derived at pairing, stored
in `SecretVault` under `device.<device_id>.push_key`, never in the plaintext
`devices` row). This keeps C's push blind while still delivering a real lock-screen
preview: **A encrypts, C relays ciphertext, the NSE decrypts.**

> **Implementation note (read-after-write).** The lifecycle event publishes
> *after* the job store write (`persist_and_publish`, `crates/job/src/lifecycle.rs`),
> but the *assistant message* is a separate write with no ordering guarantee
> relative to job completion. A naive read can encrypt the **previous** turn's
> reply — a plausible-looking *wrong* lock-screen preview, worse than empty. Gate
> the preview on the message cursor: don't build it until the session's last
> persisted row is a message **newer than this turn's start** (or carry the
> assistant message's `ordinal` in the trigger). Use a bounded retry (fixed count +
> backoff); on expiry send the **generic placeholder**, never a prior turn's text.

### iOS client (Tauri) & NSE decryption

The app is **Tauri**: a WKWebView UI driven by `src-tauri` (Rust), with native
**Swift Tauri plugins** for device capabilities and a separate **NSE** Xcode
target. Two content layers, both E2E so neither C nor Apple sees content:

**Lock-screen preview via the Notification Service Extension (NSE)** (resolved
default): the APNs payload carries the AEAD-encrypted **short preview**
(`enc`/`n`/`kid`/`bid`). The NSE fires **only** for an *alert* push with
`aps.mutable-content: 1`, gets ~30 s, reads `bid` to select the right push key,
decrypts the preview **in-place** with native **CryptoKit `ChaChaPoly`**, and
rewrites `title`/`body`. Because the preview is inlined (no network in the NSE),
this is robust — it does **not** depend on A being reachable in the 30 s window.

**Full content via self-pull on open:** tapping the notification (or opening the
app) fetches the full thread over the E2E channel using the *existing* catch-up
path — `Frame::Subscribe { session_id, since_ordinal }` (gateway replays newer
rows; see `crates/gateway/src/channel/route.rs`) or `GET
/v1/chat/sessions/{id}?before_ordinal=…` — decrypts, and renders.

The genuinely hard part is **key access from the NSE, a separate process** from
the app:

- Main app + NSE share an **App Group** and a **Keychain access group**.
- At pairing, derive a dedicated symmetric **push key** via HKDF (distinct from the
  ephemeral Noise session keys — the NSE is too short-lived to run a Noise
  handshake). A holds the matching key.
- The app stores that key in the shared Keychain with
  **`kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`** — notifications usually
  arrive on a locked screen, so `WhenUnlocked` would make decryption fail there;
  the `…ThisDeviceOnly` variant keeps the key out of iCloud Keychain backups. With
  multi-gateway, push keys are keyed by `bid`.
  - **Conscious trade-off (not a free choice):** `AfterFirstUnlock` is a *hot,
    unlocked-class* secret — readable while the phone is later locked, which is
    precisely why the NSE can use it, but it also lowers its at-rest protection.
    The blast radius is bounded: this key decrypts only the **short preview**,
    never the transcript (which needs the Noise **static** key — keep that in a
    stronger class, `WhenUnlockedThisDeviceOnly` / Secure Enclave), and `kid`
    rotation caps exposure over time.
- AEAD framing is pinned in `aura-device-proto` (ChaCha20-Poly1305, 32-byte key,
  fresh random 12-byte nonce per message, fixed/empty AAD) + a fixture the NSE's
  Swift tests consume, so the native CryptoKit path matches A's encrypt byte-for-
  byte.
- On any failure (key missing, malformed, decrypt fail, timeout) the NSE must still
  call the content handler with the placeholder — implement
  `serviceExtensionTimeWillExpire()`.
- **Forward-secrecy trade-off:** a static push key has no per-message FS; a ratchet
  would force app↔NSE ratchet-state sync across the process boundary. Pragmatic
  default: **static key in phase 1**, with a `kid` epoch field reserved in the
  payload so periodic rotation needs no format change.

## End-to-end flows

### Pairing (one-time)

```
operator: web dashboard / `aura device pair` → QR { rendezvous_url (C), code C }
app: scan → SPAKE2 with C over C's rendezvous → derive K (C never sees C/K)
   → over K: swap X25519 static keys; A hands relay_node_id + direct candidates;
             A issues a PENDING device auth_token (inert)
   → app sends {apns_token, apns_env, device_id} to A; A registers it with C's push
   → derive HKDF push key; store in shared Keychain (AfterFirstUnlockThisDeviceOnly, keyed by bid)
operator: `aura device approve <code>` → row approved, auth_token activates
```

### Turn completion → push → pull

```
agent UserChat turn completes → JobLifecycle::Completed (broadcast, kind=UserChat)
A push dispatcher → encrypt preview → POST push.gateway_url /notify
                    { instance_key, device_id (bid), collapse_id=bid:session_id, kid, enc, n }
C push → sign ES256 JWT → POST api.push.apple.com (alert, mutable-content, priority 10)
APNs → device: alert push carrying the encrypted preview blob
device: NSE picks key by bid → CryptoKit decrypts preview in-place ── then on open:
        app self-pulls: try direct (LAN/Tailscale/reverse proxy), else C blind relay
        → Noise → Frame::Subscribe{since_ordinal} / GET /v1/chat/sessions/{id}
        → decrypt → render (minimal in phase 1)
```

## Configuration

```jsonc
// aura.json (the user's gateway A)
"push":   { "enabled": true, "gateway_url": "https://remote.aura.example", "instance_key": "…" },
"relay":  { "enabled": true, "base_url": "wss://remote.aura.example/r",     "instance_key": "…" },
"direct": { "enabled": true, "advertise": ["wss://aura.lan:8889", "wss://aura.tailnet.ts.net:8889"] }
```

```
// C's push component own secret store (NOT the user's vault):
//   apns.p8 (PEM bytes), apns.key_id, apns.team_id, bundle_id
```

A does **not** store the `.p8` — only `push.gateway_url` + `instance_key`. The
`apns.*` namespace is relevant only if an operator self-hosts C with their *own*
app (the per-operator-app escape hatch).

## Crate / service / app layout

```
crates/aura-wire/                       # pure Frame/Message wire types + rmp codec (new; extracted from aura-channels)
crates/aura-device-proto/               # SPAKE2 + Noise + HKDF push key + AEAD framing + fixtures (new)
crates/model/                           # ApprovalDecision relocated here (edit)
crates/channels/                        # re-export wire types from aura-wire (edit)
crates/store/src/device.rs              # DeviceStore + DeviceRow; DevicePairingStore (new)
crates/storage/src/libsql/device.rs     # LibsqlDeviceStore + LibsqlDevicePairingStore (new)
crates/gateway/src/auth/channel.rs      # AuthedClient::Device variant (edit)
crates/gateway/src/api/device/          # /v1/device/pair WS + registration (new)
crates/gateway/src/push/                # push dispatcher: subscribe JobLifecycle → POST C (new)
crates/gateway/src/relay/               # outbound control conn to C + data legs + direct Noise endpoint (new)
crates/job/src/lifecycle.rs             # add kind: JobInputKind to JobLifecycleEvent (edit)
crates/cli/src/commands/device.rs       # aura device pair|approve|list|revoke (new)

aura-remote-host/                       # operator-run shared infra; one Cargo workspace (new)
  crates/common/  crates/admission/     #   instance registry / admission (machine-to-machine)
  crates/push/                          #   holds .p8; ES256 JWT; /notify; APNs; token pruning (own binary)
  crates/relay/                         #   blind byte-pipe + SPAKE2 rendezvous + NAT coordination
  crates/dashboard/  web/               #   ops metadata-only dashboard (+ React frontend)

app/mobile/ios/                         # SEPARATE Cargo workspace (root [workspace] exclude); Tauri (new)
  src-tauri/  core/ (aura-mobile-core)  #   Tauri app + plain-Cargo client core (no UniFFI)
  frontend/  plugins/  gen/apple/       #   React UI + Swift plugins + Xcode project incl. the NSE target
```

> Dependency direction follows the repo convention (trait in `aura-store`, libsql
> impl in `aura-storage`, business logic in its own crate). `aura-remote-host`
> crates are **operator infrastructure**, not part of the `aura` binary or its
> default-members. The iOS crates are a **separate Cargo workspace** so the root
> `cargo clippy --all` zero-warning gate never compiles an iOS/Tauri-configured
> crate for the host.

## Build order (delivery sequencing)

The full milestone plan (M0 protocol → M1 gateway pairing/E2E → M2 remote host →
M3 iOS app in Simulator → M4 real APNs/device) is in
[`todo/mobile/phase1.md`](todo/mobile/phase1.md). Key property: **only M4 needs an
Apple Developer account / real device**; everything before it is host- and
Simulator-testable (`simctl push` validates the NSE decrypt offline). Protocol +
integration risk is front-loaded into M0–M2, driven by a Rust test client before
the iOS toolchain is in the loop.

## Constraints

- **Push is mandatory; relay is operated but content prefers direct.** Push is the
  cheap necessary piece (on-demand POST, no persistent connection); the relay
  carries pairing always but content only as a fallback (direct-first; full
  hole-punch deferred).
- **C is blind.** The relay sees only Noise ciphertext + a routing id; push sees
  only `device_id → APNs token` + an **opaque AEAD preview blob it cannot decrypt**.
  Never adopt the Home Assistant plaintext-proxy pattern.
- **"Auth" on C = instance admission only** (machine-to-machine). Device auth +
  plaintext live only on A. The dashboard shows metadata only.
- **E2E ends are P ↔ A.** A sees plaintext by design; "E2E" defends against us and
  Apple, not the operator's own server.
- **URLs are configurable** (default-public, overridable to self-hosted) and
  **gated by per-instance admission keys** with rate limits.
- **Never delete sessions.** Device revoke / dead-token pruning unbinds devices and
  tokens only; session rows, transcripts, and channel bindings stay (project rule).
- **APNs env is tracked per device** (sandbox vs production). `400 BadDeviceToken` /
  `410 Unregistered` prune the token (honor the `410` timestamp).
- **Push must be a visible alert** (`apns-push-type: alert`, priority 10); silent
  `content-available` is throttled and does not launch the NSE. Payload ≤ 4 KB.
- **NSE key access requires** App Group + shared Keychain access group +
  `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`; the NSE always falls back to the
  placeholder on failure/timeout. Decrypt is native CryptoKit (no Rust).
- **No raw identifiers in logs/traces** (observability rule): hash `user_id` /
  `device_id` in dispatcher and C logs; never log APNs tokens, the `.p8`, instance
  keys, or pairing codes.

## Decisions (resolved)

Resolved 2026-06-22:

- **App framework → Tauri**, with a **simple webview UI** in phase 1. Removes
  UniFFI: the Rust core runs in `src-tauri`, the NSE uses CryptoKit, so no Swift
  consumes the core and no `xcframework` is built.
- **Operator infra → one `aura-remote-host`** (push + relay + rendezvous +
  admission + dashboard), with the `.p8`-holding **push** kept independently
  deployable (blast-radius). Replaces the earlier two-service split.
- **Connectivity → coordinator-mediated.** P and A both connect to C; content is
  **direct-first with relay fallback**; the **relay/rendezvous is in scope from
  phase 1**. Full NAT hole-punching + iroh are deferred.
- **Push content shape → encrypted preview + NSE.** A encrypts a short preview with
  the device push key; C relays the ciphertext blind; the NSE decrypts in-place;
  full content is self-pulled on open.
- **Push filter → real user turns** (`shape == Turn && kind == UserChat`); excludes
  Cron, System/background-compression, Spawned, SubagentNotification, and `/compact`
  maintenance. (There is no `Compression` `JobInputKind`; `/compact` is a
  `UserChat`-input `Maintenance`-shape job — hence the `shape` gate.)
- **Device approval → explicit `aura device approve`.** Pairing lands a `pending`
  device whose `auth_token` is inert until approval.
- **PAKE → SPAKE2**, over C's rendezvous (the untrusted-channel case where a short
  code pays off); swappable internal detail of `aura-device-proto`.
- **Multi-gateway → supported in phase 1** (per-binding `device_id`, `bid`-keyed
  NSE key selection, gateway-mediated APNs token registration; C correlates
  bindings via the shared APNs token — accepted metadata leak).
- **Push key → static in phase 1**, no ratchet, `kid` epoch reserved for later
  rotation.
- **Rust core split → `aura-wire` (extracted wire types) + `aura-device-proto`
  (shared crypto/pairing)**; iOS crates in a separate Cargo workspace.

## Related

- [`todo/mobile/README.md`](todo/mobile/README.md) — the phase roadmap (P1–P5).
- [`todo/mobile/phase1.md`](todo/mobile/phase1.md) — the phase-1 execution plan.
- [gateway.md](modules/gateway.md) — admin bearer + channel-token auth,
  `/v1/channel-ws`, `/v1/chat/*` REST, the `Frame` wire protocol this reuses.
- [channels.md](modules/channels.md) — `Channel`/`ChannelType`, `Subscribed` vs.
  `Multiplexed`; iOS registers as a `Subscribed` `ChannelType::ios`.
- [pairing.md](modules/pairing.md) — the **channel** pairing gate (orthogonal);
  this reuses its code-generation + janitor-sweep + CLI patterns for **device**
  pairing.
- [storage.md](modules/storage.md) — the store-trait / libsql-adapter split the
  `devices` table follows; plain `DELETE`, no tombstones.
- [security.md](modules/security.md) — `SecretVault` (AES-256-GCM) for A's Noise
  static key, per-device push keys, and any self-hosted-push `apns.*` credentials.
- `docs/modules/agent.md`, `crates/job/src/lifecycle.rs` — the
  `JobLifecycle::Completed` broadcast the push dispatcher subscribes to.
- `docs/web-chat.md` — the thin-client chat data-flow the iOS app mirrors.
