# channels Protocol Refactor — Connection / Subscription model

## Status

Proposed, 2026-05-11. Supersedes the "two-flavor registry" section of
[`channels.md`](./channels.md) once landed. Migration is a single flag
day across `crates/channels`, `crates/agent`, `crates/gateway`,
`crates/tui`, `channel-src/{telegram,weixin}`, and `sdks/channel-ts`;
`PROTOCOL_VERSION` bumps in lockstep.

## Why

Today `Channel` collapses two unrelated responsibilities into one
type:

1. **Protocol surface** — "telegram is a way users reach the agent".
   One per `ChannelType`.
2. **Client connection** — "this WebSocket is alive and can carry
   frames". Process-level instance.

`ChannelRegistry` papered over the collision with two disjoint views
(`sidecars` vs `session_clients`) and an `owned_session: Option<...>`
discriminator on `Channel`. The seam works for the existing transports
because each transport happens to need exactly one of the two flavors:

- Telegram / Weixin / Discord: a single subprocess bot serves every
  user of that channel — 1 process, 1 channel-type slot, multiplexed
  semantics.
- TUI: every `aura tui` invocation owns its own session — 1 process,
  1 session, no fan-out.

A web chat page does not fit either side. The browser is the protocol
surface "the web" (one of), but a single user opens many tabs, each
tab can navigate between sessions over its lifetime, and several tabs
of the same user may want to view the same session live. None of
"sidecar mode (1 per type)", "session-scoped mode (1 per session_id)",
or "session_id baked into Register" model this naturally.

Bolting "multi-attach session-scoped clients" onto the existing
registry would unblock the web case at ~500–700 LOC but entrenches
the conflation: tabs would register as N session-scoped *channels*
sharing one id, when they are really N *connections* of one channel.
Past that, every future channel that wants multiple concurrent
clients (mobile companion app, second browser session for the same
operator, multiplayer dashboard) would have to pretend to be a fleet
of session-scoped channels. The refactor cuts the conflation now.

## New model

Three concepts replace one:

| Concept          | Cardinality                          | Lifetime                                  |
| ---------------- | ------------------------------------ | ----------------------------------------- |
| **Channel**      | 1 per `ChannelType`                  | Process. Eagerly created at gateway boot from `ChannelsConfig`; never dropped while gateway is up. |
| **Connection**   | N per `Channel`                      | One per live `/v1/channel-ws` client; lives between a successful `Register` and the WS close. |
| **Subscription** | M per `Connection` (M = 0..∞)        | Created by `Subscribe { session_id }`, dropped by `Unsubscribe` or connection close. |

A `Channel` carries:

- `channel_type: ChannelType` — the type label (`"http"`, `"tui"`,
  `"telegram"`, …).
- `kind: ChannelKind` — `Multiplexed` or `Subscribed`. Compile-time
  constant per channel; **not** a config field, because the operator
  cannot meaningfully flip it (Telegram-the-bot serves all users of
  Telegram-the-protocol by construction, and HTTP-the-browser does
  not). Names describe the connection's relationship to sessions, not
  message flow direction.
- `connections: DashMap<ConnectionId, Arc<Connection>>` — every
  client currently attached to the channel.
- `subscriptions: DashMap<SessionId, DashSet<ConnectionId>>` — only
  populated for `Subscribed`. Reverse index from session → interested
  connections so emission is O(subscribers), not O(connections).
- `approval_gate: Option<Arc<ApprovalGate>>` — one shared gate per
  channel. Approval decisions are scoped per `(call_id)` inside the
  gate; any connection subscribed to the relevant session can resolve
  a pending request, and the resolution broadcasts to every
  subscriber so concurrent UIs stay consistent.

A `Connection` carries:

- `id: ConnectionId` — gateway-minted UUID, used as the key
  everywhere the registry needs to identify *this* WS instance.
- `auth: AuthedClient` — channel-auth result. Carries the bound
  `channel_type` so the channel can refuse Register frames whose
  identity doesn't match.
- `outbound: mpsc::Sender<Frame>` — the gateway's WS sink half. The
  channel `try_send`s here; full → drop frame + signal connection to
  reconnect.
- `subscribed: DashSet<SessionId>` — owned by the connection,
  mirrored into the channel's reverse index. Lives here so disconnect
  can purge cheaply by iterating one set.

### `ChannelKind`

```rust
pub enum ChannelKind {
    /// One connection carries every session of this channel_type.
    /// Used by sidecar bots (telegram, weixin, discord) where a
    /// single subprocess multiplexes traffic for many platform
    /// users. Subscribe / Unsubscribe frames from a Multiplexed-
    /// channel connection are protocol errors.
    Multiplexed,
    /// Connections receive only the sessions they explicitly
    /// subscribe to via Subscribe / Unsubscribe frames. Used by the
    /// TUI (subscribes to its own session_id on startup) and the
    /// web chat page (subscribes to the active view, may subscribe
    /// to several at once).
    Subscribed,
}
```

A `Multiplexed` channel has no `subscriptions` map — emission iterates
`connections` directly. A `Subscribed` channel's emission iterates
`subscriptions[session_id]`, which may be empty.

### Wire protocol

`PROTOCOL_VERSION` bumps from 1 → 2; the server rejects v1 Register
frames with `RegisterAck { ok: false, reason: "protocol version
mismatch" }` (same reason string as today). No back-compat shim — all
in-tree clients ship in the same PR.

Changes to `wire::Frame`:

```rust
pub enum Frame {
    /// Client → server. First frame after the WS upgrade. No more
    /// session_id field; the channel_type is what's being claimed.
    /// The session_id field is removed entirely (not "deprecated"
    /// — the field is gone, and old senders are rejected at the
    /// PROTOCOL_VERSION gate before the parser cares).
    Register {
        token: String,
        channel_type: ChannelType,
        protocol_version: u16,
    },
    RegisterAck { ok: bool, reason: Option<String> },

    /// Client → server. Only valid for Subscribed channels; servers
    /// reject from Multiplexed connections with an Error frame.
    Subscribe { session_id: String },
    /// Client → server. Subscribed only.
    Unsubscribe { session_id: String },

    /// Server → client. Final user-visible message; carries role
    /// (user or assistant). Server now echoes inbound user messages
    /// to every subscriber (see "Server echo" below), so all
    /// subscribers reconcile against the same authoritative event.
    Message(Message),
    /// Server → client. Streaming chunk for in-flight assistant
    /// output. Identical shape to today.
    Delta { session_id: String, user_id: String, text: String },
    Notice { session_id: String, user_id: String, level: String, text: String },

    ApprovalRequested { ... },   // unchanged
    ApprovalResolved { ... },    // unchanged
    ResolveApproval { ... },     // unchanged

    /// Server → client. Sent when the channel needs the client to
    /// reset state — typically after a slow-consumer drop. The
    /// client should resubscribe and pull session history via the
    /// REST endpoint to recover.
    Reset { reason: String },
}
```

`HistorySnapshot` / `HistoryAppend` (TUI-only input ring) stay
unchanged; they ride orthogonally to channel routing.

### Routing

`agent::router` produces `(channel_type, session_id, AgentOutput)`.
The registry resolves `channel_type → Arc<Channel>`. The channel
fans out:

```text
match channel.kind {
    Multiplexed => for conn in channel.connections.values():
                       conn.try_send(frame)
    Subscribed  => let subs = channel.subscriptions.get(session_id);
                   if subs.is_empty() {
                       // Drop ephemeral; Message is persisted by the
                       // storage layer before this point, so the
                       // session history is the source of truth.
                       metrics::drop_empty_subscribers(...).inc();
                       return;
                   }
                   for conn_id in subs:
                       channel.connections[conn_id].try_send(frame)
}
```

Slow consumer (`try_send` returns `Full`): the channel drops the
frame for that connection only, increments a metric, and pushes a
`Frame::Reset` onto the connection's outbound — the client treats
that as "your live stream is stale, re-subscribe and pull history".
Other subscribers are unaffected. This matches the user-facing UX
agreed in grilling: missed Deltas are recovered via REST history
fetch on reconnect; live correctness wins over guaranteed delivery
of every Delta token.

### Server echo

Inbound user messages flow agent-ward through the existing pipeline
(`SecurityGateway` → `SessionManager::append_session_message` →
agent). After persistence, the channel emits the user message as
`Frame::Message(role = user)` to every subscriber, including the
sender. Reasons:

- Tab1 and Tab2 viewing the same session see the user's own message
  through the same code path, eliminating client-side optimistic /
  reconcile divergence.
- Tab1 reloading the page recovers from REST history; that history
  is exactly what tab2 saw live. Single source of truth.
- The `ApprovalResolved` frame already used this fan-out shape, so
  the pipeline gains nothing new structurally.

The sender does not need a synthetic optimistic render — the echo
arrives within an RTT and is rendered through the same component as
other-tab inbound. (If the echo is delayed and the user notices, the
fix is to surface a "sending…" spinner on the composer, not to
duplicate render paths.)

### Approval gates

One `Arc<ApprovalGate>` per channel, registered when the channel is
constructed at boot and shared across all of that channel's
connections. The gate is identical to today's; the only model change
is that `ApprovalGateMap` collapses from "two scopes
(per-channel-type vs per-session)" to "one scope (per-channel-type)"
because session-scoped channels no longer exist.

Wire surface stays the same: `ApprovalRequested` fans out to every
connection subscribed to the call's `session_id`; the first
`ResolveApproval` wins inside the gate; `ApprovalResolved` fans out
to the same subscriber set so dismissed UIs stay consistent.

### Auth

`AuthedClient` keeps its three existing variants and adds a fourth:

| Variant              | Channel binding                       | Origin                                                |
| -------------------- | ------------------------------------- | ----------------------------------------------------- |
| `Tui`                | `"tui"`                               | Vault-issued token from `gateway.tui_token`           |
| `Subprocess { .. }`  | `bound_channel_type` (telegram/…)     | `ChannelSpawner::spawn` injects per-spawn token       |
| `Tool { label }`     | rejected on `/v1/channel-ws`          | tool sidecars                                         |
| `Web`                | `"http"` (only)                       | Admin API `POST /v1/chat/session` mints a short-lived channel-token; admin bearer never reaches `/v1/channel-ws` directly |

`RESERVED_CHANNEL_TYPES = &["http"]` keeps its current meaning —
"subprocess sidecars cannot claim it" — but `AuthedClient::Web` is
allowed past the gate because its identity is unforgeable (admin
bearer authorized the mint).

A web channel-token is bound to one workspace and is *not* bound to
a single session at mint time; the same token can `Subscribe` to any
session of the http channel. Per-session scoping was considered and
rejected: it would require the admin API to mint a new token every
time the user clicks a session in the sidebar, with no security gain
(the admin token used to mint is already workspace-omnipotent).

Token TTL: 1 hour, refreshable from the same admin endpoint without
creating a new session. The web client refreshes proactively at 50
minutes and reconnects in place. A revoked-token rejection mid-WS is
treated identically to other disconnects — exponential-backoff
reconnect, fetch a new token if 401 persists, then resume.

### Registry

`ChannelRegistry` collapses to a single map. Two-flavor split (and
its `owned_session: Option<...>` discriminator on `Channel`) is gone.

```rust
pub struct ChannelRegistry {
    channels: DashMap<ChannelType, Arc<Channel>>,
    approval_gates: Arc<ApprovalGateMap>, // collapsed: only per-channel-type
}

impl ChannelRegistry {
    pub fn install(&self, channel: Arc<Channel>) -> Result<()>;
    pub fn get(&self, channel_type: &ChannelType) -> Option<Arc<Channel>>;
    pub fn list(&self) -> Vec<ChannelType>;
    pub fn approval_gates(&self) -> Arc<ApprovalGateMap>;
}
```

`install` is called once per channel at gateway boot. There is no
runtime `unregister` for channels themselves — only for connections,
which is a `Channel`-internal operation:

```rust
impl Channel {
    pub fn attach(&self, conn: Arc<Connection>);
    pub fn detach(&self, id: ConnectionId);     // also purges subscriptions
    pub fn subscribe(&self, id: ConnectionId, session_id: SessionId) -> Result<()>;
    pub fn unsubscribe(&self, id: ConnectionId, session_id: &SessionId);
    pub fn subscribers_for(&self, session_id: &SessionId) -> SubscriberIter<'_>;
    pub fn connections(&self) -> ConnectionIter<'_>; // Multiplexed path
}
```

`agent::router::dispatch` becomes:

```rust
let channel = self.channels.get(&channel_type)?;
match channel.kind() {
    Multiplexed => for c in channel.connections() { c.try_send(frame.clone()); },
    Subscribed  => for c in channel.subscribers_for(&session_id) {
                       c.try_send(frame.clone());
                   },
}
```

The old "session-scoped wins over sidecar" precedence rule disappears
along with the dual store.

### Configuration

`ChannelsConfig` already declares one struct per known channel with
an `enabled` flag (`cli`, `telegram`, `discord`, `http`, `weixin`).
The refactor adds nothing new there; the gateway boot path is
extended to iterate the enabled set and call `ChannelRegistry::install`
for each — using `ChannelKind` constants chosen by the channel
implementation, not the config.

Future third-party channels (slack, custom internal protocol) are
config-first: the operator declares the channel in `aura.json` (a
generic `channels.extras: HashMap<String, ExtraChannelConfig {
enabled: bool, kind: ChannelKind }>` slot added in this refactor), and
their sidecar joins as a `Connection` on a `Channel` the gateway
already created. There is no longer a "first sidecar to dial in wins
the slot" race.

### Lifecycle interaction with `SidecarSupervisor`

`SidecarSupervisor` (`crates/gateway/src/sidecar/supervisor.rs`)
continues to spawn telegram/weixin bot subprocesses, but it does
**not** create channels. Channels are created at gateway boot from
config; the subprocess connects to `/v1/channel-ws` and is admitted
as a `Connection` against the pre-existing channel. If the gateway
boots with `telegram.enabled = true` but the subprocess fails to
start, the channel exists but has zero connections; agent emissions
to telegram sessions are dropped (Multiplexed with no connections),
which matches the documented "drop ephemeral, history is the source
of truth" rule. Recovery is automatic when the supervisor restarts
the bot.

## Migration

PR ordering (each PR keeps `cargo test --workspace` and
`pnpm --filter ... test` green at its tip):

1. **R-0** — this design doc. No code.
2. **R-1** — refactor `crates/channels`: new types, new wire frames,
   bump `PROTOCOL_VERSION`, rewrite registry. **Breaks the
   workspace** at this commit; R-2 / R-3 land in the same PR or
   immediately after to restore green.
3. **R-2** — `crates/agent/src/router.rs` and
   `crates/gateway/src/channel/{handshake,route}.rs` to the new
   model. Adds `ConnectionId` minting and Subscribe-frame routing.
4. **R-3** — `crates/tui` and `channel-src/{telegram,weixin}` to v2.
   Regen `sdks/channel-ts` via `ts-rs`. Workspace builds green again.
5. **R-4** — `AuthedClient::Web` + `POST /v1/chat/session` token
   mint + admin chat REST endpoints + `utoipa` openapi snapshot
   update.
6. **R-5** — web SPA `/chat/*` route group, `ChatShell`, WebSocket
   transport, components.
7. **R-6** — e2e tests (`gateway/tests/channel_ws.rs` +
   `integration-tests` + a minimal web smoke).

Step 2–3 can ride in one PR if the diff stays reviewable; the
constraint is "no commit on `master` leaves the workspace
unbuildable", not "one PR per item".

## Alternatives considered

### A. Multi-attach session-scoped clients

Keep the two-flavor registry; relax the session-scoped slot from `1`
to `N` per `session_id`. ~500–700 LOC, wire protocol unchanged,
channel-sdk unchanged. Rejected because:

- Entrenches the conflation between protocol surface and client
  connection. Every future "channel with multiple concurrent
  clients" repeats the workaround.
- "Two tabs of the same browser" registers as two channels claiming
  the same session in the type-scoped registry — a model the rest
  of the system reads as "two TUIs racing", which is misleading in
  logs, metrics, and tooling.
- The wire protocol's `Register { session_id }` already mixes "I
  serve this channel" with "I want this session"; multi-attach makes
  the mix permanent.

### B. Half-refactor: only `http` follows the new model

Leave telegram/weixin/TUI alone; add an in-gateway `http` adapter
that internally manages its own connection pool and exposes a
distinct `/v1/chat/*` WS endpoint with a different frame shape.
~300–400 LOC. Rejected because:

- Two parallel chat flows (sidecar wire vs http JSON) need parallel
  approval, blob, dedup, history, and slash-manifest plumbing.
- The next "needs multi-client" channel hits the same fork.
- The eventual clean refactor doubles in cost (now there's a second
  half to migrate).

### C. TUI model for web (every tab is a fresh session)

Drop the "session list + resume" requirement: each `/chat` visit
opens a fresh session, refresh discards it, no left sidebar. Zero
backend changes. Rejected because the user explicitly wants resume
and listing; this is the constraint that forced the multi-attach
problem in the first place.

## Out of scope

- Multi-user (end-user, non-admin) chat. The `Web` auth variant is
  scoped to "admin-issued token only" by design; adding scoped
  end-user tokens is a separate trust-boundary change.
- Persisted per-channel emission queue. `Message` is persisted via
  the storage layer before fan-out, so `Reset` + history fetch
  covers all client-recoverable cases; persisting `Delta` /
  `Notice` would compete with the session store as a source of
  truth.
- Cross-channel subscriptions (a web tab subscribing to a telegram
  session's stream). The admin `traces` view already serves the
  read-only cross-channel debugging use case; adding it as a
  first-class wire subscription is a separate UX decision.

## References

- [`channels.md`](./channels.md) — current (pre-refactor) module doc.
- [`gateway.md`](./gateway.md) — the only in-tree transport.
- [`session.md`](./session.md) — `SessionManager` ownership of
  history.
- [`security.md`](./security.md) — `SecurityGateway` placement
  relative to inbound echo.
