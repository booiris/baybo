# pairing — Per-Channel User Pairing Gate

## Problem

Channel sidecars (Telegram today, future Discord / Slack / HTTP bots)
forward every inbound user message to aura over the WS channel. The
gateway used to run it straight through `ChannelSessionResolver →
router → agent loop` — no per-user gate, anyone who could reach a
provisioned bot could drive the agent.

Aura (not the sidecar) now decides who may talk to a given bot. The
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
`@aura-staging-bot`, not yet on `@aura-prod-bot`). Keying on the pair
alone would force one decision across every bot.

Same Telegram user under two different bots ⇒ two rows. Approving on
one bot does not imply approval on the other.

### Wire addition

`Frame::Message` grew an optional `bot_id: String` field (defaults
to `""` on the wire, omitted on serialize when empty). Additive — no
`PROTOCOL_VERSION` bump. The sidecar fills it in when it knows which
bot originated the inbound event; the TUI leaves it empty. The
Telegram sidecar already tracks `botByUser` internally, so surfacing
`bot_id` on inbound is a one-line change in `pushInbound`.

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
deleted_at   INTEGER           -- soft-delete (revoke)
PRIMARY KEY (channel_type, bot_id, user_id)
UNIQUE INDEX idx_channel_pairings_code
  ON channel_pairings(code) WHERE deleted_at IS NULL
```

Soft-delete matches the rest of the libsql tables (see
`CLAUDE.md` §"Soft Delete").

### Expiry

A pending row carries an `expires_at` stamp computed at insert time:
`created_at + 15 minutes`. The TTL is a hardcoded constant
(`PENDING_TTL_SECONDS` in `crates/pairing/src/service.rs`) — long
enough for a human operator to notice a Telegram buzz and run `aura
pair approve`, short enough that a one-time curious user's code
doesn't linger in libsql for days.

Expiry only applies to `pending` rows. On approval, `status` flips
to `approved` and `expires_at` is cleared (`NULL`) — approved
pairings don't auto-expire; they stay live until `aura pair revoke`
soft-deletes them.

Behavior on an expired row:

- **Inbound from the same triple** → the service treats an expired
  pending row as if it weren't there and overwrites it with a fresh
  code + fresh `expires_at`. The user sees a new code in their
  Notice; the old one silently becomes invalid.
- **`aura pair approve <old_code>`** → returns "not found", nothing
  is mutated. The operator asks the user to message the bot again to
  mint a fresh code.
- **`aura pair list`** → expired rows surface with `STATUS=EXPIRED`
  so the operator can see the queue honestly. They're not filtered
  out; the row still occupies the triple until the user retries (at
  which point it's overwritten).

Rows are never deleted by a background sweep. An expired row is
harmless — the triple is unreachable via `approve`, and a new
inbound will overwrite in place. A future `aura pair prune` can
clean them up if operators complain about libsql growth.

### Code format

6 characters from the ambiguous-free alphabet
`ABCDEFGHJKMNPQRSTUVWXYZ23456789` (no `0/O/1/I/L`). 31 symbols × 6
positions ≈ 887 M combinations. At 100 concurrent pendings the
birthday collision is ~6 × 10⁻⁶. Collisions retry up to 8
generation attempts before surfacing an error.

Codes stay stable for the row's lifetime — once a pending row has a
code, concurrent inbound messages from the same user see the same
code (the store's upsert keeps the existing code on live-row
conflict, provided `expires_at > now`). A revoke, an expiry, or an
operator-driven prune followed by a new inbound mints a fresh code.

### Gate flow

```
            sidecar                         aura gateway
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
              │                                      `aura pair approve ABC123`"
              │                                     drop message (no session)
              │                                           │
              │◄────────────────── Frame::Notice ─────────┘
```

The refusal path does **not** create an aura session or a
`channel_sessions` row. Nothing lands in libsql for the user beyond
the pending pairing itself.

### CLI

```
aura pair list [--pending | --approved]
aura pair approve <code>
aura pair revoke <channel> <bot_id> <user_id>
```

`list` default shows every live row, newest first. Output columns:
`STATUS CHANNEL BOT USER CODE CREATED_AT EXPIRES_AT`. `STATUS` is
computed on read (`pending` + past TTL → `EXPIRED`); `EXPIRES_AT` is
blank for approved rows.

`approve` is positional on the code (not the triple) — short
typable codes are the whole reason the code column exists. If no
live pending row carries the code, returns `not found`.

`revoke` is positional on the triple so `aura pair revoke telegram
prod-bot tg_…` is explicit about what it removes.

Even the operator's own first use flows through the pending-request
path: the operator messages the bot, reads the code out of the
returned Notice, and runs `aura pair approve <code>`. Keeps one
code-path for everyone; no "trust me" escape hatch.

All three run under `retry_on_busy` (CLI shares the libsql file
with a potentially-running gateway; a `database is locked` is a
logged retry, not an operator-facing error).

### Crate layout

Per the project convention documented in `docs/modules/storage.md`,
store traits and row types live in `aura-storage` alongside the
libsql adapter; business logic lives in a dedicated crate. The split
here is:

```
crates/pairing/                 # business logic only
├── Cargo.toml
└── src/
    ├── lib.rs      // re-exports
    ├── service.rs  // PairingService (gate check + approve)
    ├── code.rs     // generate_code + generate_unique
    └── error.rs    // PairingError

crates/storage/src/channel_pairing.rs   // ChannelPairingStore trait
                                         // + ChannelPairingRow + PairingStatus
crates/storage/src/libsql/channel_pairing.rs  // LibsqlChannelPairingStore
```

Dependency direction: `aura-pairing → aura-storage`, matching
`aura-session → aura-storage` for `SessionStore`. `aura-storage`
gains no new dependency; the trait sits next to the other store
traits.

```
aura-storage ──► model, trace, job, security (defines ChannelPairingStore + row)
aura-pairing ──► model, storage              (PairingService + code gen)
aura-gateway ──► pairing, storage, …
aura-cli     ──► storage                     (CLI talks to store directly)
```

The gateway consumes `PairingService` (service) and
`ChannelPairingStore` (trait, from storage, to hold the `Arc`). The
CLI consumes only the trait — `list/approve/revoke` are
thin-wrapper store calls, so pulling in the full service would be
dead weight.

### Test support

`aura-pairing` ships no in-memory store fake: gateway tests
exercise the gate end-to-end through the real libsql adapter (via
`build_test_deps`), and the adapter has its own per-method unit
tests. A fake can be added later if service-level tests need one —
nothing does today.

### Integration points

- `crates/gateway/src/channel/state.rs` — `WsChannelState` carries
  `pairing: Arc<PairingService>`.
- `crates/gateway/src/channel/route.rs` — `enforce_pairing` runs
  in the empty-`session_id` branch of the `Frame::Message` handler,
  before `session_resolver.resolve_or_create`. On
  `CheckOutcome::Pending { code }` it sends a `Frame::Notice`
  (`level: "warn"`) and drops the inbound; on `CheckOutcome::Approved`
  it falls through.
- `crates/gateway/src/server.rs` — `GatewayDeps` carries
  `channel_pairing_store`, wired from `Store::channel_pairing`.
  `build_channel_router` constructs the `PairingService`, which owns
  the hardcoded TTL.
- `src/runtime.rs` — `ManagerGraph` carries `channel_pairing_store`;
  `build_bot_registry_deps` returns it too so the CLI can reach it.
- `src/main.rs` / `src/gateway_cmd.rs` — plumb the store through
  from `Store::open` to the CLI context / `GatewayDeps`.
- `crates/cli/src/cli.rs`, `dispatch.rs`, `commands/pair.rs` —
  `aura pair` subcommand family.
- `crates/channels/src/wire.rs` + `sdks/channel-ts/src/generated/`
  (regen) — `Frame::Message` carries optional `bot_id`.
- `channel-src/telegram/src/channel.ts` — passes `botId` on the
  `pushInbound` call site (already tracked in `botByUser`).

### TUI

TUI sends its own `session_id` on the Register frame and never hits
the empty-`session_id` branch in `route.rs`. The gate only applies
inside that branch, so TUI bypasses pairing entirely — it is local
and implicitly trusted. Same reasoning as the slash-account-auth
doc's `PrincipalSource::Cli` branch.

### Observability

- Refusals log at `warn` with `%channel_type`, `%bot_id`, hashed
  `user_id` (follows the `docs/modules/` observability rule — no
  raw identifiers in traces). The hash is a truncated 4-hex digest
  of `DefaultHasher(user_id)` — enough to disambiguate concurrent
  pendings in a log without leaking the raw id.
- Approvals (via CLI) log at `info` with the same hashed form.
- The refusal Notice's code is not logged — it is surfaced to the
  end-user verbatim and belongs only in the libsql row.

## Constraints

- Gate is aura-side only. Sidecars forward everything; the gate is
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

- `crates/channels/src/wire.rs` — `Frame::Message` carries optional
  `bot_id`.
- `crates/gateway/src/channel/route.rs` — gate lands right before
  `session_resolver.resolve_or_create` in the empty-session branch.
- `channel-src/telegram/src/channel.ts` — sends `bot_id` on the
  inbound path.
- `docs/modules/storage.md` — store convention that dictates where
  the trait + row types live.
- `docs/todo/slash-account-authorization.md` — parallel
  authorization work for slash commands. Pairing gates *messages*;
  slash-account-auth gates *operator commands typed in chat*. The
  two can share the `(channel_type, bot_id, user_id)` principal
  shape later.
