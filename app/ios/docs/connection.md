# Connection lifecycle

*The chat-leg connection supervisor — governs `app/mobile/ffi/src/transport/`
(`mod.rs`: shared wire primitives + the seams + the `SessionRegistry` facade;
`supervisor.rs`: the lifecycle actor; `pump.rs`: the hot path;
`tests.rs`: the loopback suite), the per-leg dialers in
`../mobile/ffi/src/relay/chat.rs` / `../mobile/ffi/src/direct/chat.rs`, and the Swift half of the
state machine in `App/Core/ChatStore.swift` (connState, the dial
continuations, the send paths).*

Most sentences here are scar tissue from one production bug: the 2026-08-16
cold-start send black hole, where sends returned `Ok` into a leg the gateway
silently dropped, the UI said connected, and only an app relaunch healed it.
The architecture below is shaped so that whole class cannot recur; change it
only with the scars in view.

## The shape

One **supervisor task per leg registry** (relay and direct each have one) owns
every connection-lifecycle decision. Everything reaches it as a message on one
unbounded queue:

```
FFI calls (reply oneshots)      pump events            timers / dial children
Open / Send / Preconnect /      PumpEnded{leg_id}      AckTimedOut{leg_id,
Disconnect / Unsubscribe /      SubscribeAcked{            attempt, parked_at}
ResolveApproval                     leg_id, session}   DialFinished{leg_id, …}
        └──────────────────────────┴──────────────────────────┘
                        Supervisor (one loop, no locks)
          leg: Idle | Dialing{adopters, latecomers} | Live{leg_id, …}
          sessions: session → { sink, phase: Registered
                                      | Subscribing{on_leg, attempt, waiters}
                                      | Proven{on_leg} }
```

The **pump** (one task per live socket) deliberately stays out of the loop: it
routes inbound frames straight to per-session `FrameSink`s, answers the
gateway's keepalive `Ping` locally, and runs the 45s inbound-liveness
watchdog. Exactly two read-mostly surfaces are shared outside the loop, each
with a single writer:

* the **routing map** (pump reads per frame; supervisor writes on
  open/unsubscribe/disconnect), and
* the per-leg **`last_inbound` cell** (pump writes on EVERY socket yield —
  *before* decoding, so locally-answered keepalives count; supervisor reads it
  only in the ack-timeout judgment).

The dial seam is an **owned object** (`Arc<dyn LegDialer>`, handed to
`SessionRegistry::new` once): the supervisor dials from spawned children,
which cannot borrow the leg. Both dialers re-read their credentials per call
(relay: keychain pairing record; direct: the shared `DirectHttpSlot` cache).

## The invariants (each one is a scar)

**Leg death is handled exactly once.** `Supervisor::leg_death(leg_id)` no-ops
unless `leg_id` is the current live leg — so a duplicate `PumpEnded`, a death
racing a deliberate teardown, and two discovery channels reporting the same
corpse all collapse into one transition. The pre-supervisor code needed an
`AtomicBool` fence and four hand-ordered abort sites for this; the ordering is
now structural.

**`on_disconnected` is a delivery guarantee.** It is the only thing that arms
the Swift redial ladder; a session that never hears it wedges on a
`.connected` it can't leave (that is the black hole). Death is discovered on
three channels, all funneling into `leg_death`:

1. the pump's own tail (`PumpEnded` — its socket ended);
2. a failed enqueue onto the pump's closed channel (covers a pump that died
   *without* reporting: a panic, or an abort landing mid-poll);
3. the health probe in `Preconnect` (a corpse found on foreground).

The ONE exception is `Disconnect` (logout/rebind): it tears the leg down
*without* the fan-out so a deliberate teardown doesn't kick the reconnect
ladder against credentials that were just wiped — pinned by
`a_deliberate_disconnect_never_fires_on_disconnected`.

**The fan-out is targeted, and never at a mid-redial dial.** An `Open`
withdraws its session's leg binding and installs its sink FIRST; only then can
its enqueue discover a corpse. So the death it triggers is announced to the
*other* riders — telling the reopening session its OLD leg died would make
Swift distrust the fresh dial it is about to prove (the
`lastDisconnectGeneration` fence would refuse the claim). Pinned by
`a_death_found_by_a_reopen_spares_the_reopening_session`.

**Sends are admitted per leg, not per pump.** A send is enqueued only while
`session.phase` rides the CURRENT live leg (`Subscribing` counts — the socket
is serial, so the message lands behind its `Subscribe`). "A sink exists and a
pump is alive" is not enough: after a leg death, a foreground `preconnect`
installs a fresh leg that subscribes NOTHING, and a send `Ok`'d onto it is
silently dropped by the gateway as not-subscribed. Refusal surfaces
`NotConnected` (→ `BayboError.NotConnected`), which Swift answers by falling
through to the dial-and-send slow path. The gateway side additionally NACKs
such drops with an error `Notice` (once per session per connection) instead of
staying silent.

**Acks are leg-scoped.** `SubscribeState` reaches the supervisor tagged with
the `leg_id` of the pump it arrived on, and only proves a `Subscribing` phase
parked on that same leg. A stale open resuming late can therefore neither
prove nor clobber a fresher leg's binding — the bug class the pre-supervisor
code guarded with a ptr-equality re-insert check.

**Enqueueing is not connecting.** `connect` replies only after the gateway's
`SubscribeState` (scar of commit `84667591`: a queued Subscribe used to count
as connected while the socket was a black hole). On the ack timeout
(`SUBSCRIBE_ACK_TIMEOUT`, 8s), the judgment reads the leg's `last_inbound`
cell:

* the leg carried traffic while the subscribe waited (keepalives count) → the
  leg is healthy and this one subscribe was rejected or lost (e.g. a
  cross-channel session the gateway answers with a `Notice` and no bundle):
  fail this open only (`NotConnected`), never the leg;
* the leg carried nothing → it is a half-open black hole: `leg_death`, so
  every rider redials (`SessionClosed` for the open itself).

Either way the session's phase drops to `Registered`, so the send gate refuses
until a subscription is re-proven — an unacked subscribe must never admit
sends (pinned by `a_send_after_an_unacknowledged_subscribe_is_refused`). The
`attempt` counter fences a timer to the exact park it was armed for.

**Dial coalescing with per-batch retry.** Opens and preconnects arriving
while a dial is out park on it: `adopters` (present at dial start) share its
failure; `latecomers` (arrived during) share exactly ONE fresh dial — recovery
beyond that is the client's redial ladder. A `DialFinished` for a dial the
teardown superseded closes its socket (on its own task) instead of installing
an orphan, and the dial child carries a send-on-drop report so a panic inside
`establish` (foreign dial code has panicked before) can never strand the
supervisor in `Dialing` with held replies.

## What stays OUTSIDE the supervisor (and when that changes)

The api legs (`relay/api.rs` + `leg_pool.rs`, direct's plain HTTPS) and the
blob legs are deliberately NOT under the supervisor. The criterion is the
failure model, not "is it a connection": the supervisor exists for the one
class whose failure is SILENT and whose state is DISTRIBUTED — long-lived,
server-side subscription state, a push direction, callback-driven recovery.
Api/blob legs have none of that: every use is a caller-awaited
request/response, so a dead leg fails the request in hand and the error
propagates immediately; their correctness concern is replay convergence
(`should_retry` / `ReplayPolicy`), which the supervisor cannot help with; and
the pool has its own lifecycle (two-clock staleness, TTLs, `.background`
invalidation) whose worst case is one failed request retried on a fresh leg.
That retry is route-specific: absolute/idempotent writes use `Converges`, while
issue creation uses `post_json_once` / `ReplayPolicy::Never` because it carries
no source key and a committed replay would mint a second card.

The rule: **if a leg ever grows push semantics, a subscription, or any
cross-request server-side state, it moves under the supervisor** — do not
grow a parallel set of fences. The one real coupling today is capacity, not
lifecycle: parked api-pool legs and chat reconnects share relay connection
slots (see `MAX_POOLED_LEGS`).

## The Swift half

`connState` has four states and few writers on purpose:

* `.connected` is written in exactly ONE place — `claimConnected(gen:)`, the
  shared success continuation of both dial paths (`startConnect` and
  `sendWhenReady`'s dial-and-send). The claim yields to a disconnect that
  superseded the dial (`generation`) and to a death report for the dial's own
  leg (`lastDisconnectGeneration`) — claiming there would erase the heal
  `pumpDisconnected` wrote, and with the callback consumed, nothing would ever
  exit `.connected` again (door A of the black hole).
* `.offline` is written in exactly one place — `dialFailed(gen:)`.
* `.connected` has two exits: `pumpDisconnected` (the FFI's guaranteed death
  callback) and `legLost()` (the registry's `NotConnected` verdict arriving on
  a believed-connected send / sweep resend / stop — evidence beats state).

The two dial ENTRY paths stay separate on purpose: `startConnect` coalesces
into an existing dial task, while `sendWhenReady` supersedes it (its message
must ride the new dial behind its Subscribe); they also differ in notice
clearing and `reconcileOutboxOnConnect(justSent:)`. Only the continuations are
shared — do not merge the entries.

## Testing

`transport/tests.rs` drives the real supervisor + pump against a
loopback WS server (`Server`), with two `#[cfg(test)]` seams on the message
enum: `InjectProvenForTest` (a bystander riding the leg without a full
connect — the server may be deliberately silent) and `AbortPumpForTest` (a
corpse that can't report its own death). Timing-sensitive judgments inject
millisecond ack budgets via `with_subscribe_ack_timeout` rather than sleeping
production seconds. The Swift fence/fallthrough behaviors are pinned by
`Tests/ChatStoreStaleLegTests.swift` with the fake client's `failPlainSend` /
`stallConnect` / `stallSendAfterConnect` / `dropLeg` knobs.

## The project sink

`Frame::ProjectChanged` is **session-less**, and that is what shapes its route
through the pump. Session-bearing frames fan out to whichever transcript sink
is subscribed to that session; a session-less one has nobody to fan out to, so
it fell through the catch-all into `route_per_session`, which broadcasts to
every subscribed transcript — none of which can use it — and reached nobody at
all when no chat was open, i.e. exactly when a board is on screen.

So it gets a **consuming lane** ahead of the catch-all: the pump hands it to
`SharedProjectSink` and returns, and the frame never enters the per-session
fan-out. A session-less `Gap` nudges the same sink, because a board has no
other way to learn an invalidation was dropped.

On the Swift side `ProjectEventsRelay` hops to the main actor and publishes to
`ProjectInvalidations` — a small broadcaster, not a call into `ProjectsStore`.
Three surfaces can be watching one board at once (the cards root, the board,
and an open card with a run sheet over it), and a store that reached into the
others to nudge them would make every new surface an edit to every existing
one. The relay publishes; whoever is on screen listens.

**Every scope means dirty.** A move emits no `board`-scope frame at all — it
records a timeline entry, and entering In Progress a run — so a client that
refetched only on `board` would miss precisely the change it most needs to
draw. The scope is carried only so a card page can ignore what belongs to
another number.
