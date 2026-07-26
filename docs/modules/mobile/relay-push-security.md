# Mobile Relay and Push Security

This document describes the current mobile companion to gateway relay and push
flows. It covers protocol details, authentication, post-auth communication,
security proof sketches, explicit boundaries, and the transparency properties of
the forwarding host.

The pairing handshake is documented separately in
[`pairing-security.md`](pairing-security.md). This document includes the
scan-to-pair bootstrap, then treats successful pairing as the precondition for
the later relay and push sections: P and A have authenticated the
`Noise_XXpsk0_25519_ChaChaPoly_SHA256` transcript, exchanged static public keys,
and derived the per-binding `push_key` from the handshake hash.

## Roles

| Role | Entity | Trust assumption |
|---|---|---|
| **A** | The user's Baybo gateway | Trusted endpoint. Holds conversation plaintext, the device row, the gateway Noise static private key, and each device's `push_key`. |
| **P** | The iOS companion app | Trusted endpoint. Holds the device Noise static private key, the gateway static public key, and the `push_key`; its NSE decrypts push previews locally. |
| **C** | The operator-run `remote-host` | Untrusted forwarding and push infrastructure. It performs relay admission, rendezvous matching, rate limiting, and APNs delivery, but must not learn chat plaintext, the pairing secret, Noise private keys, or encrypted-preview plaintext. |
| **APNs / Apple** | Apple Push Notification service | Delivery infrastructure. It sees APNs tokens, generic notification payloads, ciphertext preview fields, and delivery metadata, but does not hold the `push_key`. |

C is a multi-tenant host. It has no Baybo account model; its relay tenant key is
`remote_api_key`. That key is used for relay admission and relay quotas. Push
HTTP routes are keyless at the caller layer: APNs bindings and notifications are
authorized by the device→gateway delegation chain, not by admission. The relay key
is not an end-to-end authentication secret between P and A.

## Protected Assets

The design protects:

- Chat content and the contents of `wire::Frame`, including `session_id`, message
  text, attachment references, and subscription cursors.
- The pairing secret, the 256-bit QR `s=` value used as the XXpsk0 PSK.
- A and P Noise static private keys.
- The post-pairing `push_key`.
- The device's `auth_token` and the local `SecretVault` contents. The gateway
  stores only `sha256:<hex>` of that token, so the bearer itself exists on the
  device and nowhere else once pairing completes.
- Push preview plaintext, meaning the `title` and `body` that the lock screen
  shows after NSE decryption.

C is allowed to see:

- `remote_api_key`, because the relay WebSocket routes are admitted by C.
- Relay route identifiers: pairing `rendezvous_id`, content `relay_node_id`, and
  C-minted `relay_key`.
- Relay connection source address, connection time, close time, byte lengths,
  timing, and traffic class (`chat`, `api`, or `blob`).
- Push registration metadata: `device_id` (a 32-byte Ed25519 public key, hex),
  APNs token, APNs environment, and the binding-authentication fields
  (`gateway_pubkey`, `delegation`, `sig`, `counter`).
- Push `/notify` metadata: `device_id`, `bid`, `collapse_id`, `enc`, `n`,
  the gateway `sig`, the replay `counter`, and ciphertext length.

`collapse_id` is an opaque short hash of `(device_id, session_id)`, **not** the
raw `device_id:session_id`. So C learns neither the cleartext `session_id` (which
stays protected on every leg) nor anything beyond a stable per-conversation
coalescing key.

The current protocol does not attempt to hide the remaining metadata fields. In
particular, the relay is not length-hiding, and the push path does not hide that
a notification occurred.

## Scan-to-Pair Bootstrap

Relevant code:

- Operator command: `crates/cli/src/commands/device.rs`
- A-side pairing host: `crates/gateway/src/channel/relay_pair.rs`
- A-side XXpsk0 driver: `crates/gateway/src/channel/device_pair.rs`
- P-side scanner UI: `app/ios/App/Screens/ScanView.swift` (QR payload parse: `app/ios/ffi/src/qr.rs`)
- P-side pairing pump: `app/ios/ffi/src/relay/pairing.rs`

The scan flow exists to deliver one high-entropy secret out of band and to bind
the user-visible pairing action to a live cryptographic transcript.

### QR creation on A

1. The operator runs:

```text
baybo device pair [--relay-url <host>] [--remote-api-key <key>]
```

2. If an approved device already exists, the CLI prints it and asks the operator
   whether to continue. The default is no. The old device is not revoked at this
   point; replacement happens only if the new pairing completes.
3. `DevicePairingService::mint` creates:
   - `rendezvous_id`: a public UUID used as C's pairing rendezvous key.
   - `secret`: a fresh 32-byte `PairingSecret`, used as the Noise XXpsk0 PSK.
4. The CLI normalizes the relay endpoint. A bare host defaults to `wss://`; no
   daemon-served local pairing route is used.
5. The CLI builds this QR payload:

```text
baybo://pair?h=<relay-endpoint>&r=<rendezvous-id>&s=<hex-32-byte-secret>&k=<remote-api-key>
```

Field meanings:

- `h`: relay base WebSocket URL. This is not secret. It is also bound into the
  Noise prologue to prevent cross-relay splicing.
- `r`: public rendezvous id. C routes on it.
- `s`: 256-bit secret, hex-encoded. This is bearer credential material and must
  be shown only as QR content.
- `k`: relay admission key for C. It is quota/admission material, not the E2E
  authentication secret.

The CLI renders only the QR. It does not print `s=` as text, does not write it
to disk, and does not fall back to a shortened or typeable code if QR rendering
fails. A short fallback would be offline-crackable by a hostile relay.

6. The CLI opens and continuously re-hosts the gateway pairing leg:
   `GET /pair/host/{rendezvous_id}` with `x-remote-api-key: <k>`.
   This is self-contained in the `baybo device pair` process; `baybo gateway
   start` does not need to be running for pairing.

### QR scanning on P

1. The mobile UI opens the camera scanner and accepts QR codes only.
2. The scanner parses only `baybo://pair` URLs with both `r` and `s`. There is no
   manual pairing-code fallback.
3. If `h` is absent, the app uses the built-in default relay endpoint. The
   current CLI includes `h`.
4. Immediately after a valid scan, the app calls the FFI `pair_begin` method
   (`BayboClient::pair_begin`) with `endpoint`, `rendezvous_id`, `secret`, and
   optional `remote_api_key`.
5. `pair_begin` dials C's app-side pairing route:
   `GET /pair/join/{rendezvous_id}` with `x-remote-api-key: <k>`.
6. P decodes `s` and requires exactly 32 bytes. Anything shorter or malformed is
   rejected before the handshake starts.
7. P loads or creates its long-term Noise static identity from the keychain, plus
   a separate Ed25519 push identity; the `device_id` is
   `device-<hex(ed25519 pubkey)>`, so re-pairing the same physical device keeps a
   stable identity.
8. P sends `Hello { rendezvous_id, msg=e }`, starting XXpsk0 as initiator with
   the QR secret as PSK.

### Human confirmation

1. A receives `Hello`, claims the in-memory slot by `rendezvous_id`, loads the
   slot's `secret`, and continues the XXpsk0 handshake.
2. P sends `HandshakeFinal`; the `DeviceHello` body is authenticated as the msg3
   payload and includes `device_id`, APNs token, and APNs env.
3. Both sides derive the same confirmation code from the Noise handshake hash
   `h`.
4. The phone UI displays the code returned by `pair_begin`.
5. The CLI waits until A publishes the same code on the slot, then prints the
   device id and confirmation code to the operator.
6. The phone user and the operator both confirm. Either side can decline.
7. A sends `GatewayWelcome` only after both confirmations are affirmative.
8. On success:
   - A atomically approves the new device row and revokes any previous approved
     device row.
   - A stores the per-device `push_key` in `SecretVault`.
   - A persists APNs registration material for retry.
   - A returns `auth_token`, gateway push public key, `relay_node_id`, and
     relay settings in the sealed welcome.
   - P stores the paired record and its device Ed25519 identity in its private
     keychain, and the `push_key` in the shared App Group keychain.

Security property: scanning transfers a 256-bit PSK out of band and binds it to
a live XXpsk0 transcript plus human comparison. C may see `h`, `r`, and `k`, and
may carry all handshake bytes, but it does not see `s`. Without `s`, C cannot
complete the handshake or make both screens show a valid confirmation for an
attacker-chosen static-key substitution.

## Authentication Flow

There are five distinct authentication layers. They protect different things and
must not be conflated.

### 1. Scan-to-pair bootstrap: QR-carried PSK and user presence

The scan step is the first authentication boundary. It delivers the only secret
that C must not learn and ties the later cryptographic pairing to an explicit
user action on both devices.

Authentication-oriented scan sequence:

1. The operator runs `baybo device pair`.
2. A mints a fresh public `rendezvous_id` and a fresh 32-byte `PairingSecret`.
3. A renders the QR payload:

```text
baybo://pair?h=<relay-endpoint>&r=<rendezvous-id>&s=<hex-32-byte-secret>&k=<remote-api-key>
```

4. P scans the QR and accepts it only if it is a `baybo://pair` URL carrying both
   `r` and `s`.
5. P rejects `s` unless it decodes to exactly 32 bytes.
6. P dials C's `/pair/join/{rendezvous_id}` route with `x-remote-api-key: <k>`.
7. A has already opened or is re-opening C's `/pair/host/{rendezvous_id}` route
   with the same admission key.
8. C matches those two relay legs by the public `rendezvous_id`. C sees `h`, `r`,
   and `k`, but never receives `s` on the wire.
9. P and A run XXpsk0 using the QR `s` value as the PSK.
10. P and A derive the same confirmation code from the Noise handshake hash.
11. The phone user and the operator compare that code and both must accept.
12. A sends `GatewayWelcome` only after both confirmations are affirmative.

Security property: the QR scan transfers a high-entropy PSK out of band. Relay
admission proves only that both legs may use C; the QR `s` value is what
authenticates the pairing transcript against a hostile relay. The human
confirmation then authorizes the resulting device binding.

### 2. Relay admission and keyless push authorization at C

Every relay WebSocket route uses the `x-remote-api-key` header, defined by
`remote-host-protocol` as `REMOTE_API_KEY_HEADER`:

- `GET /pair/host/{rendezvous_id}`
- `GET /pair/join/{rendezvous_id}`
- `GET /control`
- `GET /content/join/{relay_node_id}`
- `GET /content/host/{relay_key}`

C resolves the header through its admission layer. Unknown or expired keys get
`401 Unauthorized`. Admitted keys are used for connection caps, bandwidth
buckets, IP/rendezvous abuse controls, and live-connection kicks on admission
reload.

The push routes carry no `remote_api_key`:

- `POST /register`
- `POST /notify`

`/register` authenticates a device's APNs binding with a device→gateway Ed25519
delegation plus the gateway's signature over the binding. `/notify` authenticates
against the delegated gateway public key stored at register and a strictly
increasing replay counter. The push device-token store is keyed by `device_id`
alone, bounded against registration churn, and both push routes sit behind the
per-source-IP request backstop before body parsing or signature verification.

Security property: relay admission authenticates a relay tenant to C. Push
authorization authenticates a device binding and each notify request to the
delegated gateway key. Neither layer authenticates P to A, authenticates A to P,
or encrypts content. A leaked `remote_api_key` lets an attacker burn relay quota
or attempt relay joins, but does not reveal chat plaintext, does not enable
pairing MITM, and does not authorize push binding or notification.

### 3. Pairing-derived endpoint identity

During device pairing, P and A run `Noise_XXpsk0_25519_ChaChaPoly_SHA256` over
C's blind pairing rendezvous:

```text
P -> A   Hello { rendezvous_id, msg=e }
A -> P   HandshakeReply { msg=(e, ee, s, es) }
P -> A   HandshakeFinal { msg=(s, se), payload=DeviceHello }
          both enter transport mode and share handshake hash h
P -> A   Sealed(DeviceConfirm)     # the phone user's accept/decline
A -> P   Sealed(GatewayWelcome)    # routing, auth_token, gateway push pubkey
P -> A   Sealed(DeviceDelegation)  # device authorizes the gateway push key
```

`DeviceConfirm` carries the phone user's pairing decision; `DeviceDelegation`
(sent after the welcome, because it signs over the gateway push key the welcome
carries) authorizes A's push key to manage the device's APNs binding at C. Both
are best-effort tails of an otherwise-complete pair: a device confirmed by both
humans is approved even if the delegation never arrives (push is then disabled
until re-pair).

The QR splits routing from authentication:

- `rendezvous_id` is public and visible to C.
- `secret` is a 256-bit PSK carried only in the QR and never sent over C.

The final handshake hash `h` commits to the prologue, both ephemerals, the PSK
mix, and both static public keys. P and A derive both the human confirmation code
and the `push_key` from `h`.

Security property: after successful confirmation, P has the real gateway static
public key, A has the real device static public key, and both share a `push_key`
that C cannot compute.

### 4. Post-pairing content authentication with Noise IK

Every chat or blob data leg runs `Noise_IK_25519_ChaChaPoly_SHA256` after C
splices the two WebSocket legs:

1. P is the IK initiator. It knows A's static public key from pairing.
2. P sends IK msg1 over the relayed data leg.
3. A is the IK responder. It uses its gateway static private key from
   `SecretVault`.
4. A reads the initiator static from the handshake state.
5. A calls `DeviceStore::lookup_approved_by_pubkey`.
6. If the static key does not belong to an approved device row, A aborts and does
   not send msg2.
7. If the row exists, A sends msg2 and both sides enter Noise transport mode.

No bearer token rides the content relay leg. The authentication gate is the
approved device static key learned during pairing.

Security property: C cannot impersonate A because it lacks A's static private
key, and cannot impersonate P because it lacks P's static private key and is not
listed in A's approved device row.

### 5. Push registration and encrypted-preview authentication

Push has three separate protections:

- C authenticates the **binding** with a per-device Ed25519 delegation chain, so
  a caller cannot touch another device's binding even without an admission key
  (see [Shared relay key tenancy](#shared-relay-key-tenancy-and-push-binding-authentication)).
- C applies per-source-IP request limiting, per-device `/notify` limiting, replay
  counters, and a bounded device-token store to keep the keyless routes bounded.
- P authenticates preview content locally with the `push_key` AEAD tag.

The binding is owned by the device's Ed25519 identity: `device_id ==
device-<hex(device_pubkey)>`, so it self-certifies at C. At pairing the device signs
a **delegation** authorizing A's gateway push key, and A signs every `/register`
and `/notify`. The signed byte layout is pinned in `device-proto`'s `delegation`
module; C re-implements verification against it.

Registration:

1. P obtains an APNs device token from iOS.
2. During pairing, P sends `device_id`, APNs env, and the APNs token (if iOS has
   already delivered it) in the authenticated `DeviceHello`. A advertises its
   gateway push public key in `GatewayWelcome`; P then sends a sealed
   `DeviceDelegation` (the 6th pairing message) signed under its device identity.
3. A persists the approved device row, stores the `push_key` and the verified
   delegation in `SecretVault`, and persists the APNs registration material it
   received (usable for retry only when the token is non-empty).
4. Before A's first push to a device in a gateway run, A best-effort POSTs a
   **signed** `/register` from its persisted APNs material:

```json
{
  "device_id": "device-<hex(ed25519 pubkey)>",
  "apns_token": "...",
  "env": "sandbox",
  "gateway_pubkey": "base64(gateway ed25519 pub)",
  "delegation": "base64(device→gateway signature)",
  "sig": "base64(gateway signature over this register)",
  "counter": 173000000000000000
}
```

   C verifies `device_id == device-<hex(device_pubkey)>`, the delegation under the
   device key, the register signature under `gateway_pubkey`, and that `counter`
   strictly exceeds the device's last accepted one; then it stores `device_id ->
   { apns_token, env, gateway_pubkey, last_counter }`. The app no longer registers
   directly with C (it holds no gateway push key). Instead the app sends its
   current APNs token to the gateway through the device API
   `POST /v1/mobile/apns-token`; the gateway persists it and re-registers
   (signed) on its next push when the token changed — so a token that arrived
   after pairing, or rotated later (reinstall / restore / new device), is bound
   without a re-pair.

Notification:

1. A encrypts the preview JSON with ChaCha20-Poly1305 under the 32-byte
   `push_key`, and signs the `/notify` with the gateway push key under a fresh
   `counter`.
2. A sends ciphertext, nonce, signature, and counter to C's `/notify`.
3. C resolves the binding by `device_id`, verifies the notify signature against
   the stored `gateway_pubkey`, rejects a non-advancing `counter` (replay), rate
   limits per `device_id`, signs the APNs provider JWT, and forwards the
   ciphertext.
4. P's Notification Service Extension reads `baybo.push-key.<bid>` from the
   shared keychain access group and verifies/decrypts the ciphertext locally.

Security property: C can decide whether to send a notification, but cannot read
or generate a valid encrypted Baybo preview without `push_key`, and cannot
register, redirect, suppress, replay, or notify a device's binding without the
gateway's push signing key, which it never holds.

## Relay Communication Flow After Authentication

Relevant code:

- P transport: `app/ios/ffi/src/relay/chat.rs` (generic frame pump: `app/ios/ffi/src/transport.rs`)
- P crypto/session core: `app/ios/ffi/src/core/content.rs`
- A relay control manager: `crates/gateway/src/channel/relay_content.rs`
- A Noise responder: `crates/gateway/src/channel/device_content.rs`
- C relay protocol: `remote-host/crates/protocol/src/relay.rs`
- C blind broker: `remote-host/crates/relay/src/broker.rs`
- C WebSocket handlers: `remote-host/crates/relay/src/serve.rs`

Post-pairing state on P:

- `relay_url`
- `remote_api_key`
- `relay_node_id`
- P Noise static private key
- A Noise static public key

Post-pairing state on A:

- Approved device static public key
- `relay_url`
- `remote_api_key`
- `device_id`
- A Noise static private key

Control and data-leg setup:

1. A's `relay_content` manager polls for an approved device row. When one exists
   and has relay settings, it dials C's `/control` endpoint using the row's
   `relay_url` and `remote_api_key`.
2. A sends `ControlHello { relay_node_id }` as the first control WebSocket frame.
   The `remote_api_key` stays in the dial header.
3. C registers that control connection under `relay_node_id`, scoped to the
   admitted `remote_api_key`.
4. P opens chat by dialing `GET /content/join/{relay_node_id}` with the same
   admission key. Chat defaults to `x-relay-leg-class: chat`.
5. C creates a fresh `relay_key`, records the pending content leg metadata, and
   sends `ControlSignal::OpenDataLeg { relay_key, class }` over A's control
   connection.
6. A receives the signal, dials `GET /content/host/{relay_key}` with its
   `remote_api_key`, and C verifies that this key matches the owner of the
   pending phone leg.
7. C calls `RelayBroker::join` to splice P's join leg to A's host leg. From that
   point on, C forwards opaque binary WebSocket messages.

Noise and frame loop:

1. P starts the IK handshake and sends msg1 over the spliced leg.
2. A authenticates P by approved static public key and sends msg2.
3. Both sides enter Noise transport mode.
4. P sends `Frame::Subscribe { session_id }`.
5. A wraps the Noise transport in the normal channel `FrameSource`/`FrameSink`
   path and reuses the same channel frame loop as TUI and web chat.
6. A answers with one `Frame::SubscribeState` bundle and streams live
   `SessionEvent`s; transcript recovery is P's REST sync call over the API
   tunnel (see `docs/sync-protocol.md`).
7. P sends user `Frame::Message` values with a client-generated
   `platform_msg_id` for idempotency.
8. A routes inbound frames into the normal gateway router, agent execution, and
   session persistence path.

Large frames are chunked at the plaintext record layer by
`device_proto::noise::write_chunked`. Each chunk is sealed as a Noise transport
message and reassembled by `FrameReassembler`.

Chat leg deduplication happens only on A. C cannot deduplicate by `device_id`
because it never sees the Noise plaintext or the authenticated device identity.
A installs a new chat leg in `WsChannelState.device_leg_registry` only after IK
succeeds and the `device_id` is known; the new leg aborts the stale predecessor.

## Push Communication Flow After Authentication

Relevant code:

- A push dispatcher (preview + signed register/notify): `crates/gateway/src/push/mod.rs`
- A delegation capture at pairing: `crates/gateway/src/channel/device_pair.rs`
- A token refresh endpoint (`POST /v1/mobile/apns-token`): `crates/gateway/src/api/admin/push.rs`
- P APNs token capture: `app/ios/App/AppDelegate.swift` → `app/ios/ffi/src/apns.rs` (`BayboClient::set_apns_token`)
- Delegation crypto (A signs, C verifies, byte layout pinned):
  `crates/device-proto/src/delegation.rs`, `remote-host/crates/push/src/delegation.rs`
- AEAD framing: `crates/device-proto/src/aead.rs`
- push key derivation: `crates/device-proto/src/kdf.rs`
- C push protocol: `remote-host/crates/protocol/src/push.rs`
- C push pipeline: `remote-host/crates/push/src/notify.rs`
- C push HTTP routes: `remote-host/crates/push/src/http.rs`
- C device-token store: `remote-host/crates/push/src/store.rs`
- P NSE decrypt path:
  `app/ios/NotificationExtension/NotificationService.swift`
- P NSE keychain path:
  `app/ios/NotificationExtension/PushKeyStore.swift`

The push flow has two stages: **register** the device's APNs binding with C
(gateway-signed, once per `(device, token)`), then per pushable turn **notify** —
encrypt the preview, sign it, and post it for C to forward blind.

### Registration lifecycle

C must hold an authenticated APNs binding before any preview can be delivered.
Registration is gateway-mediated and signed end to end; the app never POSTs C's
`/register` itself.

Keys (all established at pairing — see
[Shared relay key tenancy](#shared-relay-key-tenancy-and-push-binding-authentication)):

- Device Ed25519 identity `D` — `device_id == device-<hex(D_pub)>` — in P's private
  keychain.
- Gateway Ed25519 push key `G`, vault-persisted on A (`gateway.push_signing_key`),
  one per gateway.
- Per-device `push_key` (HKDF of the pairing handshake hash `h`), on both ends
  (A's `SecretVault`, P's App Group keychain) for preview AEAD.
- The device→gateway delegation (`D`-signature over `G_pub`), on A at
  `device.<device_id>.push_delegation`.

1. **iOS token capture.** P calls `registerForRemoteNotifications()` at launch;
   iOS later hands it an APNs device token. APNs tokens are not stable (they change
   on reinstall / restore / new device), so P re-fetches every launch.
2. **Pairing.** P sends `device_id`, APNs env, and the token if already issued, in
   the authenticated `DeviceHello`. A advertises `G_pub` in `GatewayWelcome`; P
   returns the sealed `DeviceDelegation`. A persists the approved row, the
   `push_key`, the verified delegation, and the APNs material it received.
3. **Signed `/register`.** Before A's first push to a device in a run, A
   best-effort POSTs a signed `/register` built from the persisted APNs material —
   skipped when the token is empty or no delegation is stored. The body carries
   `D`'s delegation, `G_pub`, A's `G`-signature over the binding, and a monotonic
   `counter`:

   ```json
   {
     "device_id": "device-<hex(D_pub)>",
     "apns_token": "...",
     "env": "sandbox",
     "gateway_pubkey": "base64(G_pub)",
     "delegation": "base64(D over G_pub)",
     "sig": "base64(G over this register)",
     "counter": 173000000000000000
   }
   ```

   C verifies `device_id == device-<hex(D_pub)>`, the delegation under `D_pub`, the
   register signature under `G_pub`, and `counter` strictly above the device's last
   accepted; on success it stores `device_id -> { apns_token, env, gateway_pubkey,
   last_counter }`. Any failure → `403`, binding untouched. Because C re-derives
   `D_pub` from the id, A's dispatcher drops any fan-out target whose `device_id`
   is not self-certifying (e.g. material persisted under a retired id prefix)
   before dialing at all — such a binding can never register or notify, so it is
   excluded (one INFO summary per process, per-target detail at debug) and its
   vault rows stay in place, inert, until the device pairs or registers again.
   A caches the last-registered token per device and registers at most once per
   `(device, token)` per run, tracking the outcome (registered / skipped for
   missing material / rejected by C). A `/notify` answered `404` (C has no
   binding for the device, e.g. it restarted and lost its in-memory store)
   invalidates that cache entry, forces a re-register from persisted material,
   and retries the push once — **but only when a register had actually landed
   this run**: after a skipped or rejected register the retry would `404`
   identically, so A surfaces a single per-target warning (carrying the register
   outcome) instead of re-dialing. A remote-host restart therefore still
   self-heals on the next push, while a binding C rejects cannot double its
   round-trips or its log lines.
4. **Token refresh.** A token that missed `DeviceHello`, or rotated after pairing,
   reaches A through the device API: P sends its current token with
   `POST /v1/mobile/apns-token`. A authenticates the device principal and
   persists it (`device.<device_id>.apns`); because the cached token now differs,
   the dispatcher re-registers (signed, as in step 3) on its next push — no
   re-pair. The app re-posts its token on every launch/foreground, so a POST
   whose token+env match the stored registration is a no-op: no vault rewrite,
   no log line above debug. Anything short of a byte-identical stored entry
   (absent, unreadable, undecodable, or differing) falls open to the write,
   which doubles as the recovery path for malformed stored material.
5. **Pruning.** When APNs rejects a token (`400`/`410`), C unbinds it (the device
   row on A is never touched); a later push re-registers from A's persisted material.

### Notify flow

Dispatch trigger:

1. A's `PushDispatcher` subscribes to the `JobLifecycle` broadcast bus.
2. A dispatches only successfully completed real user chat turns:
   `phase == Completed` and `kind == UserChat`.
3. Cron, Compact (`/compact`), Spawned, SubagentNotification, failed turns, and
   cancelled turns do not trigger push.
4. The `Completed { reply_ordinal }` event identifies the persisted assistant
   reply row. If there is no ordinal, A does not push.

Preview construction:

1. A reads the assistant text at `reply_ordinal`.
2. A truncates the body to `PREVIEW_MAX_CHARS = 200`.
3. If the row is missing, not assistant-authored, or textless, A uses the generic
   `"New message"` body instead of any stale previous reply.
4. A frames the plaintext preview as JSON:

```json
{ "title": "Baybo", "body": "...", "session_id": "..." }
```

   The `session_id` rides inside the sealed plaintext — never the outer APNs
   payload — so the app can deep-link a notification tap to its conversation
   while C stays blind to session ids. Older NSE builds ignore the field.

Encryption and `/notify`:

1. A loads `device.<device_id>.push_key` and its gateway push signing key from
   `SecretVault`.
2. A generates a fresh random 12-byte nonce.
3. A encrypts the preview with ChaCha20-Poly1305, empty AAD, producing
   `ciphertext || 16-byte tag`.
4. A signs the notify (over `device_id`, `enc`, `n`, `bid`, `counter`) with the
   gateway push key, under a fresh strictly-increasing `counter`.
5. A POSTs to C (`collapse_id` is a short hash of `(device_id, session_id)`, so
   it fits APNs' 64-byte collapse-id limit and reveals no `session_id`):

```json
{
  "device_id": "device-<hex(ed25519 pubkey)>",
  "collapse_id": "<hex(sha256(device_id || ':' || session_id)[..16])>",
  "bid": "device-...",
  "enc": "base64(ciphertext||tag)",
  "n": "base64(nonce)",
  "sig": "base64(gateway signature over this notify)",
  "counter": 173000000000000000
}
```

C to APNs:

1. C resolves the binding by `device_id` and verifies `sig` against the stored
   `gateway_pubkey`; a forged or co-tenant signature is rejected (`403`).
2. C rejects a `counter` that does not strictly exceed the device's last accepted
   one (replay), then applies per-`device_id` notification rate limiting.
3. C signs or reuses an APNs provider JWT.
4. C sends an APNs payload with a generic visible alert and the ciphertext fields:

```json
{
  "aps": {
    "alert": { "title": "Baybo", "body": "New message" },
    "mutable-content": 1
  },
  "enc": "...",
  "n": "...",
  "bid": "device-..."
}
```

NSE on P:

1. iOS invokes the Notification Service Extension because `mutable-content` is
   set.
2. The NSE reads `enc`, `n`, and `bid`.
3. The NSE loads `baybo.push-key.<bid>` from the shared keychain access group.
4. The NSE opens the ChaCha20-Poly1305 sealed box.
5. If decryption and JSON decoding succeed, the NSE rewrites the visible
   `title` and `body`.
6. On any failure, the NSE leaves the incoming notification content unchanged.

The honest C implementation sends `"Baybo" / "New message"` as the fallback
alert. If C itself is malicious and holds the APNs `.p8` plus APNs token, it can
bypass `/notify` and send an arbitrary ordinary APNs alert. That does not let it
produce a valid encrypted Baybo preview, but it is outside the preview-integrity
guarantee.

## Direct-mode push (web identity)

The sections above describe the **scan-to-pair** (Noise device) path. The
**direct** transport — connect by typing a gateway URL + admin token, no pairing
(see [`companion.md`](companion.md) and `app/ios/ffi/src/direct/`) — has
no Noise handshake to bootstrap push from, yet can still deliver lock-screen
pushes by provisioning the *same* binding over the admin-token TLS REST surface.

Relevant code:

- App registration: `app/ios/ffi/src/direct/push.rs` (`register`, exposed as
  `BayboClient::register_push`).
- Gateway endpoints: `crates/gateway/src/api/admin/push.rs`
  (`GET /v1/push/params`, `POST /v1/push/register`).
- Gateway binding store: `crates/gateway/src/push/web.rs`.
- Default remote host: `crates/gateway/src/push/mod.rs` (`DEFAULT_PUSH_RELAY_URL`).

### Flow

1. The app reuses its long-term **Ed25519 push identity** (`baybo.device-sign-key`,
   the *same* key the relay path uses, so a phone keeps one `device_id ==
   device-<hex(pub)>` whichever way it connects).
2. `GET /v1/push/params` (admin Bearer) returns the gateway's Ed25519 push public
   key `G_pub`.
3. The app **load-or-creates** a stable 32-byte `push_key` in the **shared App
   Group keychain** (`baybo.push-key.<device_id>`, minted once and reused) for its
   NSE, and signs the **same delegation** authorizing `G_pub`
   (`device_proto::delegation::sign_delegation`). Reusing the key (rather than
   regenerating per register) means the NSE never holds a key that mismatches an
   in-flight push.
4. `POST /v1/push/register` (admin Bearer) carries `device_id`, the APNs token +
   env, the `push_key` (hex), and the delegation (hex). The gateway recovers the
   device key from `device_id`, **verifies the delegation under it**, and persists
   the binding — the `push_key` / APNs / delegation under the same
   `device.<id>.*` vault names a paired device uses, plus a `web_push.<id>` meta
   record holding the remote-host endpoint (the built-in default).
5. The dispatcher enumerates these web bindings alongside approved device rows and
   `/register` + `/notify`s them through that remote host **unchanged**.

So the binding is cryptographically **identical** to a paired device's: C, the
delegation chain, the AEAD preview, and the iOS NSE are all untouched. Push is
keyless, so the gateway needs no admission key — the binding's only routing input
is the remote-host **endpoint**, hardcoded today to
`DEFAULT_PUSH_RELAY_URL = wss://proxy.baybo.space` (the public proxy the app also
defaults to for pairing). It is not yet operator-configurable; a future `[push]`
config block can override it.

### Trust-model difference (weaker than the Noise path)

One thing changes, and it is load-bearing: the `push_key` is **generated on the
device and delivered over TLS under the admin Bearer token**, not derived from a
mutually-authenticated, forward-secret Noise handshake hash `h`. So for
direct-mode previews:

- **Preview confidentiality rests on the admin token + TLS + the App-Group
  keychain**, not on Noise. There is **no forward secrecy** for the `push_key`,
  and no device-static attestation that the app is talking to the *right* gateway
  beyond TLS server-auth. An attacker who holds the gateway's **admin token** can
  register a binding (or read one it registered) and would receive the encrypted
  previews — but an admin-token holder is already a fully-trusted principal on the
  direct path (it is the gateway's master REST credential, and direct mode hands
  it to the app deliberately), so this does **not** widen the trust boundary
  beyond what direct mode already assumes. Relay's Claim 3 (Noise-derived,
  forward-secret `push_key`) does **not** hold here.
- **Binding integrity at C (Claim 4) is preserved.** The web identity still mints
  a real Ed25519 key and signs the delegation, so the per-binding delegation chain
  C verifies is exactly the same — and since push is keyless, that chain is the
  *sole* authorization for the binding (just as for a paired device). Any caller,
  even on the same blind remote host, still cannot register over, redirect,
  suppress, or spam the binding — it can forge neither the device delegation nor
  the gateway signature.
- C and APNs see the **same** metadata and **only ciphertext** previews as on the
  relay path (the `push_key` never leaves the two endpoints).

Net: direct-mode push trades the Noise path's forward-secret, device-attested
`push_key` for "type a URL + token and go", while keeping preview ciphertext
opaque to C and the binding un-hijackable (the keyless delegation chain gates it).
An operator who wants the stronger guarantee uses scan-to-pair.

## Forwarding Host Transparency

Here, transparency has two concrete meanings.

First, C is transport-transparent for relay traffic. It provides admission,
rendezvous, backpressure, and quotas, then forwards opaque bytes. The endpoint
authentication, encryption, frame decoding, session subscription, and message
routing happen inside the A/P encrypted channel.

Second, C is content-transparent for push previews. It has to build and sign the
APNs request, but the preview body it forwards is opaque ciphertext.

### Relay path

C performs:

- Admission by `remote_api_key`.
- Rendezvous matching by `rendezvous_id`, `relay_node_id`, and `relay_key`.
- Resource controls: connection caps, per-IP throttles, per-rendezvous join
  limits, frame size caps, and bandwidth classes.

C does not perform:

- `wire::Frame` parsing.
- Chat `session_id` parsing.
- Message or attachment plaintext parsing.
- Device identity resolution.
- Noise key derivation.
- Gateway device-token validation.

If C modifies a relay frame:

- During pairing, XXpsk0 transcript authentication fails.
- During content, Noise IK transport AEAD authentication fails and the session is
  closed.

If C connects P to the wrong gateway:

- P's IK msg1 is addressed to the gateway static public key learned during
  pairing.
- The wrong gateway does not have the matching static private key and cannot
  complete IK.

If C connects an unapproved phone to A:

- A extracts the initiator static from IK and rejects it unless it matches an
  approved device row.

### Push path

C sees and can act on push routing metadata, but not preview plaintext:

- C stores APNs token and env.
- C receives `enc` and `n`, but not `push_key`.
- C copies `enc`, `n`, and `bid` into the APNs payload.
- C cannot encrypt an attacker-chosen preview that the NSE will accept.
- Tampering with `enc`, `n`, or `bid` causes local decrypt failure on P.

The proof is limited to encrypted preview content. It does not claim that a
malicious APNs provider can never display any text; a `.p8` holder can send an
ordinary APNs alert. It only cannot produce a valid encrypted Baybo preview
without the `push_key`.

## Shared relay key tenancy and push-binding authentication

Relevant code:

- Pinned signing layout: `crates/device-proto/src/delegation.rs`
- C-side verification: `remote-host/crates/push/src/delegation.rs`
- C device-token store (keyed by `device_id`): `remote-host/crates/push/src/store.rs`

C is multi-tenant and its relay tenant key is `remote_api_key`. The built-in
public proxy hands out one **shared** trial key, `guest`, so many
mutually-distrusting devices use the same relay key. Push no longer carries any
admission key, which makes the important invariant explicit: the push device-token
store cannot rely on relay admission to isolate one device's APNs binding from
another's. Under `guest`, `remote_api_key` was never a sufficient tenant boundary;
on keyless push routes it is absent entirely.

The threat, absent further defense: C's `/register` would be an unconditional
overwrite by `device_id`, and `device_id` is visible to C. So any caller who
learned a victim `device_id` could overwrite the binding (redirect the victim's
encrypted previews to a device it controls), prune it (suppress the victim's
pushes), or spam `/notify` (lock-screen buzz / replay). Previews stay encrypted
throughout (no `push_key`), but delivery routing and metadata would be at the
attacker's mercy.

### Delegation chain

The binding is authenticated to its **device key**, independent of the relay
admission key:

- The device holds an Ed25519 identity key `D`. Its `device_id` *is* that public
  key (`device-<hex(D_pub)>`), so the binding self-certifies — C re-derives
  `device_id` from the key carried in the request.
- At pairing the device signs a delegation authorizing the gateway's Ed25519 push
  key `G` (carried in `GatewayWelcome`); it sends the delegation as the sealed 6th
  pairing message.
- The gateway signs every `/register` and `/notify` with `G`, including a
  strictly-increasing `counter`.

C verifies, with **no stored secret and no trust-on-first-use**: `device_id ==
device-<hex(D_pub)>`, the delegation under `D_pub`, the request signature under
`G_pub` (stored at register), and `counter` strictly greater than the device's
last accepted. Only the holder of `D` can authorize a `G`, and only the holder of
`G` can mutate or notify the binding.

### Properties and limits

- A caller, even with no admitted relay key, cannot register over, redirect,
  suppress, replay, or spam another device's binding — it can forge neither `D`'s
  delegation nor `G`'s signature.
- `device_id` is a full 32-byte Ed25519 public key, so it is not enumerable; the
  delegation makes its confidentiality non-load-bearing anyway.
- C (a separate workspace) re-implements verification against the byte layout
  pinned in `device-proto`; a pinned cross-implementation test vector guards the
  two against drift.
- The gateway is the **only** registrar (it holds `G`). A token that missed
  `DeviceHello`, or rotated after pairing (APNs tokens are not stable across
  reinstall / restore / new device), reaches the gateway through
  `POST /v1/mobile/apns-token`, and the gateway persists it and re-registers
  (signed) on its next push when it changed — no re-pair needed.
- This protects binding *integrity and delivery routing*, not the existence or
  timing of a push, nor preview confidentiality (which `push_key` already covers).

## Security Proof Sketches

### Claim 1: hostile C cannot turn pairing into durable MITM

Pairing uses `Noise_XXpsk0_25519_ChaChaPoly_SHA256`. The public
`rendezvous_id` is only a routing key. The 256-bit PSK `secret` is carried only
in the QR and never sent over C. Without the PSK, C cannot construct handshake
messages that either endpoint accepts.

The confirmation code and `push_key` are derived from the final handshake hash
`h`. That hash commits to the prologue, both ephemerals, the PSK mix, and both
static public keys.

Therefore C can deny service or steal a public rendezvous leg, but cannot
substitute static keys, steal the active device token, compute `push_key`, or
become a persistent MITM.

### Claim 2: hostile C cannot read or forge relay chat frames

Post-pairing content uses Noise IK:

- P authenticates A by the gateway static public key learned at pairing.
- A authenticates P by looking up the initiator static public key in approved
  device rows.
- C lacks both endpoint static private keys.

Noise transport AEAD protects every post-handshake message. Modification or
state replay causes decrypt failure. Connecting the wrong endpoint fails static
authentication.

Therefore the relay leg is an untrusted transport only. It affects availability
and metadata, not chat-frame confidentiality or integrity.

### Claim 3: hostile C cannot read or forge encrypted push previews

`push_key` is derived from the pairing handshake hash via HKDF-SHA256. It exists
only in A's `SecretVault` and P's App Group keychain. C and APNs do not hold it.

Each preview is encrypted with ChaCha20-Poly1305 using a fresh 96-bit nonce and
empty AAD. Assuming nonce uniqueness and AEAD security, C cannot recover
plaintext from `enc` / `n` and cannot generate a different ciphertext that opens
to attacker-chosen preview JSON. Tampering fails at the NSE and falls back to
the incoming notification content.

Therefore C can drop, delay, or replay ciphertext notifications, but cannot learn
or newly forge encrypted preview plaintext.

### Claim 4: no caller key authorizes another device's push binding

Admission (`remote_api_key`) is *not* the binding's isolation boundary — the push
routes carry no admission key, and the built-in `guest` trial relay key is shared
by many mutually-distrusting tenants anyway. Binding integrity instead comes from
the per-device Ed25519 delegation chain C verifies statelessly (see
[Shared relay key tenancy](#shared-relay-key-tenancy-and-push-binding-authentication)):
`device_id` self-certifies its device key, `/register` carries the device's
delegation + the gateway's signature, `/notify` is verified against the
`gateway_pubkey` stored at register, and both carry a strictly-increasing replay
`counter`.

Therefore a caller cannot register over, redirect, suppress, replay, or spam
another device's binding, even knowing its `device_id` — it can forge neither the
device delegation nor the gateway signature. Relay quotas remain keyed by
`remote_api_key` (and, for bandwidth, `(remote_api_key, server_id)`); the push
binding store and notify rate limiter are keyed by the globally-unique
`device_id`. End-to-end content security still comes from Noise and `push_key`,
not from C admission.

## Security Boundaries

### In scope

- C is a malicious relay: it can observe, drop, delay, reorder, inject, and
  replace WebSocket bytes.
- C is a malicious push sender: it can read `/register` and `/notify`, choose
  whether to call APNs, and tamper with APNs payloads.
- Network attackers can observe public traffic to C, while TLS still protects
  HTTP/WS transport to C.
- Other tenants — including co-holders of a **shared** relay `remote_api_key` such
  as the `guest` trial key — can try to spend that key's relay quota, guess device
  ids, call the keyless push routes for another device id, or race public
  rendezvous joins.

### Out of scope

- A or P endpoint compromise. Gateway and phone are plaintext endpoints.
- A same-UID malicious process on the gateway host reading `SecretVault`, the
  master key, process memory, or local files.
- A malicious app or extension in the same iOS keychain access group.
- APNs availability and push metadata privacy (existence/timing of a push).
- Relay length and timing privacy.
- Ordinary APNs alert anti-forgery under `.p8` compromise. If C is malicious and
  holds the provider key, it can send arbitrary non-decrypted APNs alerts. It
  still cannot generate a valid encrypted Baybo preview.

### C can do

- Reject admission, close connections, drop control signals, delay relay bytes,
  or throttle traffic.
- Steal a public pairing `rendezvous_id` join and cause handshake failure. A
  immediately re-parks the host leg, and C has per-rendezvous join limits, but
  this remains availability-only hardening.
- Observe relay byte lengths, directions, timings, source addresses, and traffic
  class.
- Store, drop, or prune APNs tokens in its push token store.
- Send the honest generic placeholder notification.
- If malicious and holding `.p8`, send arbitrary ordinary APNs alerts outside the
  encrypted-preview path.
- Leak metadata it sees, such as APNs token, device id, `remote_api_key`, and
  `collapse_id` (an opaque hash).

### C cannot do, assuming endpoint keys stay secret

- Decrypt relay chat frames.
- Modify relay chat frames and have P or A accept them.
- Silently connect P to the wrong gateway.
- Impersonate an approved device to A.
- Recover push preview plaintext from ciphertext.
- Generate a new encrypted preview that the NSE decrypts to attacker-chosen
  plaintext.
- Register over, redirect, suppress, replay, or spam a device's push binding —
  including from a caller with no admitted relay key — without the gateway's push
  signing key (the per-device delegation chain + replay counter gate it).

## Operational Notes

- `remote_api_key` leakage is a relay resource-abuse risk. It is not a content
  decryption key, not the pairing MITM defense, and — because push is keyless and
  the binding is gated by the per-device delegation chain — not a way to hijack,
  redirect, or suppress another device's push binding.
- C's `.p8` is an APNs provider credential. If it leaks, an attacker can send
  notifications to known APNs tokens, but cannot generate valid encrypted
  previews.
- When a device is revoked, A's relay-content manager observes the absence of an
  approved device row and tears down the control connection, so A stops
  advertising the `relay_node_id`.
- `/register` is sent only by A (it holds the gateway push signing key the binding
  is authenticated with); P cannot register directly, and never holds APNs
  provider credentials. P keeps the binding current by sending its APNs token to A
  via `POST /v1/mobile/apns-token`, so a token that missed `DeviceHello` or
  rotated later is re-registered without a re-pair.
- A retries push registration (signed) from non-empty APNs material persisted in
  the vault before its first push in a run, which self-heals a missing C-side
  token binding when the token was available to A. A `/notify` answered `404`
  (C's in-memory binding is gone, e.g. a remote-host restart) additionally
  invalidates A's per-device register cache and re-registers + retries inline, so
  recovery does not wait for an A restart or a token rotation.
- The push replay `counter` keeps a persisted high-water mark in the vault, so it
  stays strictly increasing across an A restart even if the wall clock steps
  backward (an NTP correction otherwise risks a counter below C's last accepted,
  which would `403` every push until the clock caught up).
- Blob transfer uses the same Noise IK authentication as chat, but on a separate
  relay data leg with traffic class `blob`; C meters it as background traffic.
  See [`blob-transfer.md`](blob-transfer.md).

## Related

- [`companion.md`](companion.md) - mobile companion architecture.
- [`pairing-security.md`](pairing-security.md) - device pairing threat model and proof.
- [`blob-transfer.md`](blob-transfer.md) - dedicated relay blob legs.
- `crates/gateway/src/channel/relay_content.rs` - A-side relay control manager.
- `crates/gateway/src/channel/device_content.rs` - A-side Noise IK responder.
- `crates/gateway/src/push/mod.rs` - A-side push dispatcher.
- `remote-host/crates/relay/src/broker.rs` - C blind relay broker.
- `remote-host/crates/relay/src/serve.rs` - C relay WS routes and admission.
- `remote-host/crates/push/src/notify.rs` - C push pipeline.
- `app/ios/NotificationExtension/NotificationService.swift` - P-side NSE decrypt path.
