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

C is a multi-tenant host. It has no Baybo account model; its only tenant key is
`remote_api_key`. That key is used for admission, quotas, and APNs binding
partitioning. It is not an end-to-end authentication secret between P and A.

## Protected Assets

The design protects:

- Chat content and the contents of `wire::Frame`, including `session_id`, message
  text, attachment references, and subscription cursors.
- The pairing secret, the 256-bit QR `s=` value used as the XXpsk0 PSK.
- A and P Noise static private keys.
- The post-pairing `push_key`.
- The gateway-side device `auth_token` and local `SecretVault` contents.
- Push preview plaintext, meaning the `title` and `body` that the lock screen
  shows after NSE decryption.

C is allowed to see:

- `remote_api_key`, because all relay WebSocket routes and push HTTP routes are
  admitted by C.
- Relay route identifiers: pairing `rendezvous_id`, content `relay_node_id`, and
  C-minted `relay_key`.
- Relay connection source address, connection time, close time, byte lengths,
  timing, and traffic class (`chat` or `blob`).
- Push registration metadata: `device_id`, APNs token, and APNs environment.
- Push `/notify` metadata: `device_id`, `bid`, `kid`, `collapse_id`, `enc`, `n`,
  and ciphertext length.

The current protocol does not attempt to hide those metadata fields. In
particular, the relay is not length-hiding, and the push path does not hide that
a notification occurred.

## Scan-to-Pair Bootstrap

Relevant code:

- Operator command: `crates/cli/src/commands/device.rs`
- A-side pairing host: `crates/gateway/src/channel/relay_pair.rs`
- A-side XXpsk0 driver: `crates/gateway/src/channel/device_pair.rs`
- P-side scanner and UI: `app/mobile/src/App.tsx`
- P-side pairing pump: `app/mobile/src-tauri/src/pairing.rs`

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
4. Immediately after a valid scan, the app calls the Tauri `pair_begin` command
   with `endpoint`, `rendezvous_id`, `secret`, and optional `remote_api_key`.
5. `pair_begin` dials C's app-side pairing route:
   `GET /pair/join/{rendezvous_id}` with `x-remote-api-key: <k>`.
6. P decodes `s` and requires exactly 32 bytes. Anything shorter or malformed is
   rejected before the handshake starts.
7. P loads or creates its long-term Noise static identity from the keychain. The
   `device_id` is derived from the public key prefix, so re-pairing the same
   physical device keeps a stable identity.
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
   - A returns `auth_token`, gateway static public key, `relay_node_id`, and
     relay settings in the sealed welcome.
   - P stores the paired record and the `push_key` in the App Group keychain.

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

### 2. Relay and push admission at C

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

The push routes carry `remote_api_key` in the JSON body:

- `POST /register`
- `POST /notify`

C resolves that key through the same admission seam. The push device-token store
is keyed by `(remote_api_key, device_id)`, so a tenant cannot read, overwrite, or
prune another tenant's APNs binding by guessing a `device_id`.

Security property: this layer authenticates a tenant to C. It does not
authenticate P to A, does not authenticate A to P, and does not encrypt content.
A leaked `remote_api_key` lets an attacker burn quota or attempt relay joins, but
does not reveal chat plaintext and does not enable pairing MITM.

### 3. Pairing-derived endpoint identity

During device pairing, P and A run `Noise_XXpsk0_25519_ChaChaPoly_SHA256` over
C's blind pairing rendezvous:

```text
P -> A   Hello { rendezvous_id, msg=e }
A -> P   HandshakeReply { msg=(e, ee, s, es) }
P -> A   HandshakeFinal { msg=(s, se), payload=DeviceHello }
          both enter transport mode and share handshake hash h
P -> A   Sealed(DeviceConfirm)
A -> P   Sealed(GatewayWelcome)
```

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

Push has two separate checks:

- C authenticates the gateway tenant with `remote_api_key`.
- P authenticates preview content locally with the `push_key` AEAD tag.

Registration:

1. P obtains an APNs device token from iOS.
2. During pairing, P sends `device_id`, APNs env, and the APNs token if iOS has
   already delivered it in the authenticated `DeviceHello` payload.
3. A persists the approved device row, stores the `push_key` in `SecretVault`,
   and persists the APNs registration material it received. That material is
   usable for retry only when the token is non-empty.
4. If the token arrives after pairing, the paired app best-effort POSTs the same
   registration body directly to C's `/register` using the relay admission key it
   already holds from the QR:

```json
{
  "remote_api_key": "...",
  "device_id": "ios-...",
  "apns_token": "...",
  "env": "sandbox"
}
```

5. Before A's first push to a device in a gateway run, A also best-effort POSTs
   `/register` from its persisted APNs material when that material is non-empty.
   This self-heals a restarted or pruned C-side token store.
6. C stores `(remote_api_key, device_id) -> { apns_token, env }`.

Notification:

1. A encrypts the preview JSON with ChaCha20-Poly1305 under the 32-byte
   `push_key`.
2. A sends ciphertext and nonce to C's `/notify`.
3. C admits by `remote_api_key`, resolves the APNs token by
   `(remote_api_key, device_id)`, rate limits, signs the APNs provider JWT, and
   forwards the ciphertext.
4. P's Notification Service Extension reads `baybo.push-key.<bid>` from the
   shared keychain access group and verifies/decrypts the ciphertext locally.

Security property: C can decide whether to send a notification, but cannot read
or generate a valid encrypted Baybo preview without `push_key`.

## Relay Communication Flow After Authentication

Relevant code:

- P transport: `app/mobile/src-tauri/src/content.rs`
- P crypto/session core: `app/mobile/core/src/content.rs`
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
4. P sends `Frame::Subscribe { session_id, since_ordinal }`.
5. A wraps the Noise transport in the normal channel `FrameSource`/`FrameSink`
   path and reuses the same channel frame loop as TUI and web chat.
6. A replays catch-up rows above `since_ordinal` and streams live
   `SessionEvent`s.
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

- A push dispatcher: `crates/gateway/src/push/mod.rs`
- AEAD framing: `crates/device-proto/src/aead.rs`
- push key derivation: `crates/device-proto/src/kdf.rs`
- C push protocol: `remote-host/crates/protocol/src/push.rs`
- C push pipeline: `remote-host/crates/push/src/notify.rs`
- C push HTTP routes: `remote-host/crates/push/src/http.rs`
- P NSE decrypt path:
  `app/mobile/apple/NotificationExtension/NotificationService.swift`
- P NSE keychain path:
  `app/mobile/apple/NotificationExtension/PushKeyStore.swift`

Dispatch trigger:

1. A's `PushDispatcher` subscribes to the `JobLifecycle` broadcast bus.
2. A dispatches only successfully completed real user chat turns:
   `phase == Completed`, `shape == Turn`, and `kind == UserChat`.
3. Cron, System, Spawned, SubagentNotification, failed turns, cancelled turns,
   and Maintenance-shaped `/compact` jobs do not trigger push.
4. The `Completed { reply_ordinal }` event identifies the persisted assistant
   reply row. If there is no ordinal, A does not push.

Preview construction:

1. A reads the assistant text at `reply_ordinal`.
2. A truncates the body to `PREVIEW_MAX_CHARS = 200`.
3. If the row is missing, not assistant-authored, or textless, A uses the generic
   `"New message"` body instead of any stale previous reply.
4. A frames the plaintext preview as JSON:

```json
{ "title": "Baybo", "body": "..." }
```

Encryption and `/notify`:

1. A loads `device.<device_id>.push_key` from `SecretVault`.
2. A generates a fresh random 12-byte nonce.
3. A encrypts the preview with ChaCha20-Poly1305, empty AAD, producing
   `ciphertext || 16-byte tag`.
4. A POSTs to C:

```json
{
  "remote_api_key": "...",
  "device_id": "ios-...",
  "collapse_id": "ios-...:session-id",
  "kid": 0,
  "bid": "ios-...",
  "enc": "base64(ciphertext||tag)",
  "n": "base64(nonce)"
}
```

C to APNs:

1. C validates `remote_api_key`.
2. C resolves `(remote_api_key, device_id)` to APNs token and env.
3. C applies per-`(remote_api_key, device_id)` notification rate limiting.
4. C signs or reuses an APNs provider JWT.
5. C sends an APNs payload with a generic visible alert and the ciphertext fields:

```json
{
  "aps": {
    "alert": { "title": "Baybo", "body": "New message" },
    "mutable-content": 1
  },
  "enc": "...",
  "n": "...",
  "kid": 0,
  "bid": "ios-..."
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
- C copies `enc`, `n`, `kid`, and `bid` into the APNs payload.
- C cannot encrypt an attacker-chosen preview that the NSE will accept.
- Tampering with `enc`, `n`, or `bid` causes local decrypt failure on P.

The proof is limited to encrypted preview content. It does not claim that a
malicious APNs provider can never display any text; a `.p8` holder can send an
ordinary APNs alert. It only cannot produce a valid encrypted Baybo preview
without the `push_key`.

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

### Claim 4: C's tenant isolation is scoped to `remote_api_key`

Relay and push both resolve `remote_api_key` through a shared admission seam.
Relay quotas are keyed by that admission key. Push APNs bindings are keyed by
`(remote_api_key, device_id)`.

Therefore one tenant cannot use its own admitted key to claim another tenant's
pending content leg, overwrite another tenant's APNs token, or prune another
tenant's binding. This is quota and binding isolation. End-to-end content
security still comes from Noise and `push_key`, not from C admission.

## Security Boundaries

### In scope

- C is a malicious relay: it can observe, drop, delay, reorder, inject, and
  replace WebSocket bytes.
- C is a malicious push sender: it can read `/register` and `/notify`, choose
  whether to call APNs, and tamper with APNs payloads.
- Network attackers can observe public traffic to C, while TLS still protects
  HTTP/WS transport to C.
- Other tenants can try to spend their own `remote_api_key` quota, guess device
  ids, or race public rendezvous joins.

### Out of scope

- A or P endpoint compromise. Gateway and phone are plaintext endpoints.
- A same-UID malicious process on the gateway host reading `SecretVault`, the
  master key, process memory, or local files.
- A malicious app or extension in the same iOS keychain access group.
- APNs availability and push metadata privacy.
- Relay length and timing privacy.
- Push anti-replay. The current AEAD payload has no timestamp, monotonic counter,
  or AAD-bound collapse id; C can replay a previously seen `enc` / `n` / `bid`
  and cause the device to show the old decrypted preview again.
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
- Replay an old encrypted preview payload.
- Leak metadata it sees, such as APNs token, device id, `remote_api_key`, and
  `collapse_id`.

### C cannot do, assuming endpoint keys stay secret

- Decrypt relay chat frames.
- Modify relay chat frames and have P or A accept them.
- Silently connect P to the wrong gateway.
- Impersonate an approved device to A.
- Recover push preview plaintext from ciphertext.
- Generate a new encrypted preview that the NSE decrypts to attacker-chosen
  plaintext.
- Use one tenant's `remote_api_key` to overwrite another tenant's APNs token
  binding.

## Operational Notes

- `remote_api_key` leakage is primarily a resource-abuse risk. It is not a
  content decryption key and not the pairing MITM defense.
- C's `.p8` is an APNs provider credential. If it leaks, an attacker can send
  notifications to known APNs tokens, but cannot generate valid encrypted
  previews.
- When a device is revoked, A's relay-content manager observes the absence of an
  approved device row and tears down the control connection, so A stops
  advertising the `relay_node_id`.
- `/register` can be sent by A from persisted pairing material or by P when iOS
  delivers the APNs token after pairing. P never holds APNs provider credentials.
- A retries push registration from non-empty APNs material persisted in the vault
  before its first push in a run, which self-heals a missing C-side token binding
  when the token was available to A.
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
- `app/mobile/apple/NotificationExtension/NotificationService.swift` - P-side NSE decrypt path.
