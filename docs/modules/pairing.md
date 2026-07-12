# pairing — Per-Channel User Pairing Gate

This document covers **channel pairing** for sidecar-routed inbound users. It is
separate from mobile **device pairing**, which is documented in
[`mobile/companion.md`](mobile/companion.md) and
[`mobile/pairing-security.md`](mobile/pairing-security.md).

## Problem

Channel sidecars (Telegram and Weixin today, future Discord / Slack / HTTP bots)
forward every inbound user message to baybo over the WS channel. The
gateway used to run it straight through `ChannelSessionResolver →
router → agent loop` — no per-user gate, anyone who could reach a
provisioned bot could drive the agent.

Baybo (not the sidecar) now decides who may talk to a given bot. The
operator approves pairings from the CLI; unknown users get a short
code and must wait for approval.

## Design

### Pairing key

Pairings are scoped to the **triple** `(channel_type, bot_id, user_id)`.

- `channel_type` — `telegram`, `discord`, …
- `bot_id` — the operator-chosen stable label already used by
  `channel_bots` (`prod-bot`, `staging-bot`, …). Empty string for
  channels / connections without a bot concept.
- `user_id` — the platform-native user identifier the sidecar sends
  on the Message frame.

The triple matters because one Telegram person might hit two of the
operator's bots with different trust levels (approved on
`@baybo-staging-bot`, not yet on `@baybo-prod-bot`). Keying on the pair
alone would force one decision across every bot.

Same Telegram user under two different bots ⇒ two rows. Approving on
one bot does not imply approval on the other.

### Wire addition

`Frame::Message` grew an optional `bot_id: String` field (defaults
to `""` on the wire, omitted on serialize when empty). Additive
change. The sidecar fills it in when it knows which bot originated
the inbound event; the TUI leaves it empty. The shared channel SDK
(`sidecars/sdk/channel-ts/src/bot.ts`) tracks `botByUser` and stamps
`botId` on every `pushInbound`, so all sidecars surface `bot_id` on
inbound for free.

Existing `user_id`, `session_id` plumbing is untouched. Outbound
Notice / Message frames stay keyed on `user_id` only — the sidecar
already maintains its own `user_id → bot_id` map for routing.

### Pairing state

`channel_pairings` libsql table:

```
channel_type TEXT    NOT NULL
bot_id       TEXT    NOT NULL  -- '' for channels with no bot concept
user_id      TEXT    NOT NULL
code         TEXT    NOT NULL  -- short human-typable
status       TEXT    NOT NULL  -- 'pending' | 'approved'
created_at   INTEGER NOT NULL
expires_at   INTEGER           -- NULL once approved; Unix seconds
approved_at  INTEGER
PRIMARY KEY (channel_type, bot_id, user_id)
UNIQUE INDEX idx_channel_pairings_code
  ON channel_pairings(code)
```

Revocation is a plain `DELETE` — there is no tombstone column.

### Expiry

A pending row carries an `expires_at` stamp computed at insert time:
`created_at + 15 minutes`. The TTL is a hardcoded constant
(`PENDING_TTL_SECONDS` in `crates/pairing/src/service.rs`) — long
enough for a human operator to notice a Telegram buzz and run `baybo
pair approve`, short enough that a one-time curious user's code
doesn't linger in libsql for days.

Expiry only applies to `pending` rows. On approval, `status` flips
to `approved` and `expires_at` is cleared (`NULL`). Approved rows
don't carry an `expires_at`, but they are **not** kept forever: the
`baybo-janitor` sweep also reaps approved rows whose `approved_at` is
older than `PAIRING_APPROVAL_TTL` (7 days) — an old approval is a
record of a one-time grant, not a live capability (access is
re-established at message-time via the channel route). `baybo pair
revoke` still deletes an approved row on demand before the TTL fires.

Behavior on an expired row:

- **Inbound from the same triple** → the service treats an expired
  pending row as if it weren't there and overwrites it with a fresh
  code + fresh `expires_at`. The user sees a new code in their
  Notice; the old one silently becomes invalid.
- **`baybo pair approve <old_code>`** → returns "not found", nothing
  is mutated. The operator asks the user to message the bot again to
  mint a fresh code.
- **`baybo pair list`** → expired rows surface with `STATUS=EXPIRED`
  so the operator can see the queue honestly. They're not filtered
  out; the row still occupies the triple until the user retries (at
  which point it's overwritten).

Stale rows are reaped by a background sweep. `baybo-janitor` runs
`ChannelPairingStore::purge_expired` on an hourly cadence
(`PAIRING_SWEEP_INTERVAL` = 1h, faster than the day-scoped log
sweeps because pending codes expire on the order of minutes); it
hard-deletes expired pending rows (`expires_at <= now`) and
TTL-aged approved rows in one `DELETE`. The sweep is wired in
production via `Janitor::with_pairing_store`. There is **no** `baybo
pair prune` CLI subcommand — the CLI surface is still only
`list` / `approve` / `revoke`; retention is the janitor's job, not
an operator command.

### Code format

6 characters from the ambiguous-free alphabet
`ABCDEFGHJKMNPQRSTUVWXYZ23456789` (no `0/O/1/I/L`). 31 symbols × 6
positions ≈ 887 M combinations. At 100 concurrent pendings the
birthday collision is ~6 × 10⁻⁶. Collisions retry up to 8
generation attempts before surfacing an error.

Codes stay stable for the row's lifetime — once a pending row has a
code, concurrent inbound messages from the same user see the same
code (the store's upsert keeps the existing code on live-row
conflict, provided `expires_at > now`). A revoke, an expiry, or a
janitor sweep followed by a new inbound mints a fresh code.

### Gate flow

```
            sidecar                         baybo gateway
              |                                   |
   user msg ─►│── Frame::Message ─────────────►   │
              │                                   │ look up (ct, bot, uid)
              │                                   │
              │                           ┌───────┴───────┐
              │                           │ approved?     │
              │                           │               │
              │                           │ yes           │ no
              │                           ▼               ▼
              │        resolve_or_create session     mint/reuse code
              │        → router → agent loop        upsert pending row
              │                                     Frame::Notice:
              │                                     "pair required, code ABC123
              │                                      ask operator to run
              │                                      `baybo pair approve ABC123`"
              │                                     drop message (no session)
              │                                           │
              │◄────────────────── Frame::Notice ─────────┘
```

The refusal path does **not** create an baybo session or a
`channel_sessions` row. Nothing lands in libsql for the user beyond
the pending pairing itself.

### CLI

```
baybo pair list [--pending | --approved]
baybo pair approve <code>
baybo pair revoke <channel> <bot_id> <user_id>
```

`list` default shows every live row, newest first. Output columns:
`STATUS CHANNEL BOT USER CODE CREATED_AT EXPIRES_AT`. `STATUS` is
computed on read (`pending` + past TTL → `EXPIRED`); `EXPIRES_AT` is
blank for approved rows.

`approve` is positional on the code (not the triple) — short
typable codes are the whole reason the code column exists. If no
live pending row carries the code, returns `not found`.

`revoke` is positional on the triple so `baybo pair revoke telegram
prod-bot tg_…` is explicit about what it removes.

Even the operator's own first use flows through the pending-request
path: the operator messages the bot, reads the code out of the
returned Notice, and runs `baybo pair approve <code>`. Keeps one
code-path for everyone; no "trust me" escape hatch.

All three run under `retry_on_busy` (CLI shares the libsql file
with a potentially-running gateway; a `database is locked` is a
logged retry, not an operator-facing error).

### Crate layout

Per the project convention documented in `docs/modules/storage.md`,
store traits and row types live in the `baybo-store` ports crate; the
libsql adapter lives in `baybo-storage` and business logic lives in a
dedicated crate. The split here is:

```
crates/pairing/                 # business logic only
├── Cargo.toml
└── src/
    ├── lib.rs            // re-exports
    ├── service.rs        // PairingService (gate check + approve)
    ├── code.rs           // generate_code + generate_unique
    ├── error.rs          // PairingError
    ├── device_service.rs // DevicePairingService (iOS device pairing —
    │                     //   see docs/modules/mobile/companion.md)
    └── device_slot.rs    // in-flight device-pairing slot DTO

crates/store/src/channel_pairing.rs      // ChannelPairingStore trait
                                          // + ChannelPairingRow + PairingStatus
crates/storage/src/libsql/channel_pairing.rs  // LibsqlChannelPairingStore
```

The crate hosts two orthogonal things: the channel-pairing gate this
doc describes, and the mobile device-pairing business logic
(`DevicePairingService` / `DevicePairingSlot`). They share no state.

Dependency direction: `baybo-pairing → baybo-store` for the trait +
row types, matching how `baybo-session` reaches `SessionStore`. The
trait sits in `baybo-store` next to every other store trait; the libsql
impl lives in `baybo-storage`, which the assembly layer wires in.

```
baybo-store   ──► model                       (defines ChannelPairingStore + row + PairingStatus)
baybo-storage ──► store, model                (LibsqlChannelPairingStore; implements the trait)
baybo-pairing ──► model, store                (PairingService + code gen; consumes the trait + row types)
baybo-gateway ──► pairing, store, storage, …  (holds the Arc<dyn ChannelPairingStore>, wires the libsql impl)
baybo-cli     ──► store, storage, pairing*    (pair commands talk to the store directly)
```

\* `baybo-pairing` is pulled in only for the orthogonal `baybo device
pair` (`DevicePairingService`), not for the channel gate.

The gateway consumes `PairingService` (service) and
`ChannelPairingStore` (trait — imported from `baybo-store`, to hold the
`Arc`). The CLI consumes only the trait — `list/approve/revoke` are
thin-wrapper store calls, so pulling in the full service would be
dead weight.

### Test support

`baybo-pairing` ships no in-memory store fake: gateway tests
exercise the gate through the real libsql adapter (the
`authorize_upload` tests in `crates/gateway/src/channel/blobs.rs`
build `LibsqlChannelPairingStore` over an in-memory pool), and the
adapter has its own per-method unit tests. A fake can be added later
if service-level tests need one — nothing does today.

### Integration points

- `crates/gateway/src/channel/state.rs` — `WsChannelState` carries
  `pairing: Arc<PairingService>`.
- `crates/gateway/src/channel/route.rs` — `enforce_pairing` runs
  in the `ChannelKind::Multiplexed` arm of `resolve_inbound_session`,
  after inbound dedup and before slash handling and
  `session_resolver.resolve_or_create`. On
  `CheckOutcome::Pending { code }` it sends a `Frame::Notice`
  (`level: "warn"`) and drops the inbound; on `CheckOutcome::Approved`
  it falls through.
- `crates/gateway/src/channel/blobs.rs` — `authorize_upload` runs the
  same `pairing.check(…)` before any attachment bytes land on disk;
  an unapproved sidecar user gets a `PairingRequiredResponse` refusal.
- `crates/gateway/src/server.rs` — `GatewayDeps` carries the whole
  `stores: Store` bundle; `build_channel_router` →
  `WsChannelState::from_deps` constructs the `PairingService` from
  `deps.stores.channel_pairing`. The pairing TTL stays a hardcoded
  const inside `baybo-pairing`.
- `crates/baybo/src/runtime.rs` — `ManagerGraph` carries the cloneable
  `stores: Store` bundle; `build_bot_registry_deps` returns
  `(Arc<SecretVault>, Store)` so the CLI reaches the pairing store as
  `stores.channel_pairing`.
- `crates/baybo/src/main.rs` / `crates/baybo/src/gateway_cmd.rs` — plumb the store through
  from `Store::open` to the CLI context / `GatewayDeps`.
  `gateway_cmd.rs` also wires it into `Janitor::with_pairing_store`
  for the hourly `purge_expired` sweep (see Expiry).
- `crates/cli/src/cli.rs`, `dispatch.rs`, `commands/pair.rs` —
  `baybo pair` subcommand family.
- `crates/wire/src/lib.rs` (re-exported as `baybo_channels::wire`) +
  `sidecars/sdk/channel-ts/src/generated/` (regen) — `Frame::Message`
  carries optional `bot_id`.
- `sidecars/sdk/channel-ts/src/bot.ts` — the shared `ChannelBot` tracks
  `botByUser` and stamps `botId` on every `pushInbound`, so every
  sidecar (telegram, weixin) surfaces `bot_id` on inbound.

### TUI

The TUI registers as a `Subscribed`-kind channel and attaches to its
session via `Subscribe` frames; the gate lives only in the
`ChannelKind::Multiplexed` arm of `route.rs`, so the TUI bypasses
pairing entirely — it is local and implicitly trusted. Same reasoning
as the slash-account-auth doc's `PrincipalSource::Cli` branch.

### Observability

- Refusals log at `warn` with `%channel_type`, `%bot_id`, hashed
  `user_id` (follows the `docs/modules/` observability rule — no
  raw identifiers in traces). The hash is a truncated 4-hex digest
  of `DefaultHasher(user_id)` — enough to disambiguate concurrent
  pendings in a log without leaking the raw id.
- Approvals produce no trace log — the CLI prints the approved triple
  (raw; it's the operator's own terminal) in its command output.
- The refusal Notice's code is not logged — it is surfaced to the
  end-user verbatim and belongs only in the libsql row.

## Constraints

- Gate is baybo-side only. Sidecars forward everything; the gate is
  the single choke point (matches the principle already in
  `docs/todo/slash-account-authorization.md`).
- Fail closed: an unknown triple is pending, not approved. An empty
  `bot_id` is still a valid key slot — do not special-case it to
  "any bot."
- Expired pending rows are fail-closed too: `approve` on an expired
  code must return `not found` without leaking whether the code
  previously existed (don't distinguish "never existed" from
  "expired" to the operator's terminal beyond a single "code
  expired or not found" line).
- Short codes are operator-facing, not secret. They appear in chat
  logs (the user sees them) and in CLI output. Don't store hashed
  — the gate needs to find the row by code, and the code is not a
  credential.
- Approvals are per-triple, never inherited. Revoking bot A does
  not revoke bot B; approving on one bot does not approve on
  another.

## Related

- `crates/wire/src/lib.rs` (re-exported as `baybo_channels::wire`) —
  `Frame::Message` carries optional `bot_id`.
- `crates/gateway/src/channel/route.rs` — gate lands in the
  `ChannelKind::Multiplexed` arm, before slash handling and
  `session_resolver.resolve_or_create`.
- `sidecars/sdk/channel-ts/src/bot.ts` — the shared ChannelBot sends
  `bot_id` on the inbound path for every sidecar.
- `docs/modules/storage.md` — store convention that dictates where
  the trait + row types live.
- `docs/todo/slash-account-authorization.md` — parallel
  authorization work for slash commands. Pairing gates *messages*;
  slash-account-auth gates *operator commands typed in chat*. The
  two can share the `(channel_type, bot_id, user_id)` principal
  shape later.
