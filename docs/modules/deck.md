# deck — agent-authored live cards

**Implementation deviations** (each deliberate, none load-bearing on the security model):

- **Card services run on the host, not under `crates/sandbox`** (post-implementation reversal, 2026-07-18). The sandbox jail broke bun binary resolution (bwrap's scrubbed `PATH`) and walled services off from what real cards need (a Codex-quota card's `~/.codex` creds + network; a machine-status card's host CLIs); since the sandbox was never a security boundary against the trusted author, it was removed rather than perforated. `service.js` and `ctx.exec` now spawn directly like the channel sidecars. The `StdinSource::Piped` + `DetachedChild::take_stdin` additions to `baybo-sandbox` were reverted with it. Full rationale in "The service runtime" below.
- **The deck shell ships in the transcript dist**, as a second Vite entry (`deck.html`) served at `baybo-transcript://localhost/deck.html` — not a separate `App/Resources/deck` copy step. One dist, two entries; `build-app.sh` and `project.yml` are unchanged, and no CI path-filter change is needed (the shell lives under the already-filtered `app/ios/web`; `crates/deck` is not an ffi dependency).
- **Provenance events are structured tracing** (target `deck::provenance`: install / update with hash before→after / delete / restore / purge / quarantine / sdk-regate), not `TraceStore` spans — deck lifecycle has no session to hang a trace tree on; store-backed spans are deferred with that design question.
- **The install-time rendered preview screenshot** (browser-sidecar best-effort, authoring step 4) is not implemented; install is the plain dry-run gate.
- **iOS surfaces**: the FFI exposes fetch/recycle/bundle/call/layout/enable/disable/delete/restore but not purge or the per-card `openapi.json` (REST-only, desktop affordances). The recycle bin has a native screen (`DeckRecycleScreen`, pushed from the Deck header's ☰ menu — the Chats-☰ grammar; rows restore via `deck_restore` and the board refetches on the `DeckChanged` echo); purge stays REST-only. The native delete confirm is a system alert rather than the hand-rolled `ConfirmDialog`.
- **Relay-leg op replay is governed per op by the card's own contract**: every op's mandatory `x-baybo-retryable` declaration (compiled at install, served as `retryable_ops` on the deck view) picks the phone's `ReplayPolicy` — a declared-safe op may replay a silent pooled leg, anything else fails up to the card (arbitrary agent-written service code is not safely at-least-once). The absolute-state deck writes stay on the convergent retry path unconditionally.

**Post-design additions (`5ad915c5`, later than the original build).** Two capabilities beyond the 2026-07-17 design landed after the fact: (1) **automatic git version history** for card bundles — `workspace/deck/` is a git repo and install/update/purge auto-commit (see "Version history is automatic" under *The card bundle*); (2) **cross-chat update discovery tools** — `DeckCardList` + `DeckCardGet`, so the agent can update a card in a fresh conversation with no memory of its uuid and no filesystem reach into the deck root (see *Authoring pipeline*). The same commit trimmed all four `DeckCard*` tool descriptions to one-liners — the bundle contract lives in the `/deck` skill, not in the always-on tool schemas that ride every cached owner-channel prompt (~326 tokens for the four tools, down from ~621).

**Post-design additions (`2026-07-18`) — per-size adaptation + optional maximize.** The original size story was skeletal: a card declared one `size`, the ⤢ cycle spanned all three grid sizes whether or not the card handled them, and real cards wrote a single fixed layout that clipped at other sizes. Three things landed to make size real, all authoritative from the manifest and refreshed on every install/update:
- **`sizes: [small|wide|large]`** on the manifest (non-empty, contains `size`; absent ⇒ `[size]`, the honest legacy default). The client's ⤢ cycle is confined to this set and a single-entry list hides the ⤢ control. Persisted as a `deck_cards.sizes` column (comma-joined; an empty legacy column reads back as `[size]`). Cards adapt with **`deck.onSizeChange(fn)`** (new SDK method; fires immediately + on every change) — one `card.html`, layout switched off `deck.size` / a self-set `[data-size]` attribute, never a reload.
- **`maximize: bool`** on the manifest. When true the shell renders a ⛶ button in the tile's top-right; a tap expands the *same iframe* to full-screen (the drag engine's `position: fixed` + placeholder trick — zero reload — with `deck.size` flipping to a fourth value `"max"`), a ✕ restores it. Native fades the wordmark header out while maximized (`DeckStore.maximized` ← a new `maximize` bridge message); the **tab bar stays** (switch tabs and back preserves the maximized card). A card must actually provide a `"max"` layout and leave the top-right/bottom clear — the skill states both.
- **Optional `src/` + host `bun build`.** `card.html` stays one self-contained built file, but a complex card may author it from TypeScript/modules under `src/` and build with `bun build` on the host. `src/` rides along with the bundle (install copies it under caps: ≤30 MB each / ≤60 MB total, no symlinks, file count unbounded; the gateway never runs or reads it) purely so **`DeckCardGet` returns it** for a cross-chat edit from the real inputs. `install`'s `materialize` went from copying four fixed files to the four files plus the `src/` subtree.

On update, the capability set (`sizes`/`maximize`) is a property of the code, so it is always refreshed from the new manifest — and if the new code dropped the size the user was on, the row's `size` clamps to the new manifest default (`set_installed` grew `size`/`sizes`/`maximize` params for exactly this; the boot re-gate clamps the same way). No wire-frame change: `DeckChanged` → `GET /v1/deck` refetch already carries the new `DeckCardDto` fields; `src` reaches the agent only through the `DeckCardGet` tool result (never a client surface).

## Overview

Deck is a dashboard tab of **live cards, each authored end-to-end by the agent on user request**. The user says "make me a card that watches my Claude quota" (or "a machine-status board") in chat; the agent writes a small self-contained bundle — a backend (`service.js`) that runs supervised on the gateway host and a frontend (`card.html`) that renders in a sandboxed iframe on the phone — and installs it with one tool call. Cards are not limited to any genre: anything the agent can express with the universal service context (external HTTP, host commands) is a valid card.

Deck replaces the **Pulse** tab, which is nothing but a renamed placeholder: commit `195c9c30` relabelled the `HomeTab.agents` slot to "Pulse" (en and zh-Hans) and its icon to `waveform.path.ecg`, still rendering `PlaceholderScreen`. There is no data source, storage, FFI surface, or test to migrate. The similarly named `SessionPulse` (`crates/gateway/src/channel/session_pulse.rs`) is the unrelated, load-bearing unread-badge broadcaster and is untouched.

Naming: **Deck** (en; the zh-Hans tab label is the owner-chosen kanban-board term, recorded in `Localizable.xcstrings`), icon `rectangle.stack`. The name is load-bearing across every layer: `crates/deck` (`baybo-deck`), `/v1/deck/*`, `Frame::DeckCardData`, `DeckSink`, `DeckScreen`/`DeckStore` (Swift), the `deck` global (card JS).

## The card bundle

A card is a directory of plain files at `workspace/deck/<uuid>/`:

```
workspace/deck/<uuid>/
  manifest.json    title + size/sizes/maximize (install-time defaults), refresh declaration, sdk stamp
  openapi.json     the card's op contract (see "admission contract")
  service.js       agent-written backend; runs on the gateway host
  card.html        agent-written frontend; runs in a sandboxed iframe on the phone
  src/             OPTIONAL pre-build sources (bun build → card.html); inert, kept for DeckCardGet
```

Plain files were chosen over blob-store versioning and over inline sqlite columns for one reason: **the agent iterates on cards with its ordinary `Read`/`Edit`/`Write` tools**, and the operator can read, hand-edit, diff, and git the bundle. Install follows `SkillInstall`'s staging discipline — validate → stage under the destination root → atomic rename (`SkillInstall` stages inside the skills dir deliberately, so the rename stays same-filesystem-atomic) — then adds the deck-specific tail `SkillInstall` doesn't have: insert the DB row, start the service.

**Version history is automatic (`repo.rs`).** `workspace/deck/` is a standalone git repo, and every bundle-*file* mutation — install, update, purge — auto-commits the touched card's directory (`deck: <event> <title> (<uuid>)`), so the plain-file rationale above pays off directly: `git -C workspace/deck log -- <uuid>/` is a card's revision history, `git show <rev>:<uuid>/service.js` recovers an old version, and a purged card's code is still reachable in history (purge commits the *deletion*, git keeps the prior revisions). The commit is scoped to the one card's dir by `card_id` (concurrent mutations can't cross-contaminate) and serialized behind a `git_lock` (no `.git/index.lock` collisions). It is **best-effort** — a missing `git`, detached HEAD, or lock failure degrades to a `deck::provenance` `warn` and never fails the deck operation, exactly like the identity-repo auto-commit in the Edit tool. Soft delete and restore touch no files, so they record no commit (the `deck::provenance` tracing already logs those transitions). The atomic-rename staging scratch (`.staging/`, holding the update-path `.old` backups) is gitignored. This is deliberately *not* the `spec_hash` version story — `spec_hash` gates re-admission; git gives the operator a rollback/diff surface.

Manifest semantics, to kill a dual-source-of-truth trap: the manifest's `title` and size class are **install-time defaults only** — after install the `deck_cards` row is authoritative, user layout edits rewrite the row, and `DeckCardUpdate` preserves the row's values instead of clobbering them from a stale manifest. The refresh declaration is `{op, min_emit_interval_secs}`: the op the dry-run gate invokes, and the floor the emit clamp enforces (an event-driven card declares its floor honestly without promising a cadence). The sdk stamp records which preamble version installed the card — the boot re-gate's trigger: a stamp differing from the current preamble sends the card back through the dry-run gate, and a pass restamps it.

`service.js` and `card.html` are the two halves of one card and never talk to each other directly. The backend produces JSON (it knows *how to ask Anthropic for quota*, or *how to read the host's load average*); the frontend turns JSON into pixels (it knows *how to draw a quota ring*). The only paths between them are validated ops and pushed snapshots, both crossing the gateway.

## The service runtime

### Process model

Every enabled card's service is an **always-resident supervised bun process**, started at gateway boot and restarted with the `SidecarSupervisor` backoff curve (500ms–30s). Resident-forever was chosen over lazy-start/idle-reap and over spawn-per-request: the design's data plane is push (services tick on their own schedule and emit), so a process with no live timer has nothing to do, and the card count cap bounds the fleet.

The process runs **directly on the gateway host — no OS sandbox** — spawned like the channel sidecars (`Command::new(bun)`, inheriting the host environment so `bun` resolves off the login `PATH` and a card can reach host state, installed CLIs, and credential dirs). This is a deliberate reversal of the original design (which ran the service under `crates/sandbox` with `NetworkPolicy::None`): sandboxing bun broke binary resolution (bwrap's scrubbed `PATH` never found `bun`) and, more fundamentally, walled the service off from exactly what real cards need — a Codex-quota card must read `~/.codex` credentials and reach the network; a machine-status card must run host CLIs. The sandbox was never a security boundary against the author anyway (see the trust model below), so it was removed rather than perforated. The remaining bounds are all app-level:

- **Effects funnel through the parent by convention, not by a jail.** `ctx.fetch` is a stdio RPC to the Rust parent (so the secret-placeholder reveal + audit keep working); the child *could* open its own socket, but the SDK gives it no reason to and the vault stays parent-only regardless.
- **Per-call timeout (30s): the call fails, the process survives.**
- Stdout/snapshot size caps (4 MB exec output, 5 MB snapshot): an oversize op result fails the call; an oversize emit is rejected and logged (only quarantine records an error face).
- A **card count cap** (64) refused at install.

### Quarantine

Restart backoff alone lets a crash-looping card burn CPU forever at the 30s floor, silently. So the supervisor keeps per-card failure counters: **5 crashes or 10 call timeouts inside a 10-minute sliding window auto-disable the card** — service stopped, `quarantined_at` stamped, the card renders an error face on the phone with a Re-enable action. A bad card degrades itself, never the gateway. (A per-card egress budget was considered and declined; timeout + quarantine is the accepted containment.)

### The SDK: runtime-injected, both sides

Agent-generated plumbing is the most likely failure mode, so **card code contains none**. The gateway never runs `service.js` directly; it spawns a **preamble** bundled in the gateway binary which owns the stdio JSON-RPC framing (`init` / `call{id,op,params}` / `result` / `emit` / capability RPCs), builds `ctx`, imports `service.js`, and dispatches into its exports. A service is pure logic:

```js
export const ops = {
  quota: async ({ provider }, ctx) =>
    parse(await ctx.fetch(URLS[provider], { headers: { "x-api-key": "[{REDACTED_SECRET_…}]" } })),
}
export function start(ctx) {
  setInterval(async () => ctx.emit(await ops.quota({ provider: "anthropic" }, ctx)), 300_000)
}
```

Card-side, the deck shell inlines its own `sdkCard.js` into each iframe's `srcdoc` ahead of `card.html`; card code sees only the `deck` global (`deck.onData(cb)`, `deck.call(op, params)`, `deck.size`, `deck.onSizeChange(cb)`) and never the MessagePort handshake. `deck.size` is `small`/`wide`/`large`/`max`, and `onSizeChange` fires immediately then on every change (resize or maximize/restore) so one document adapts without a reload. The same injected-plumbing argument covers **design**: a base stylesheet (`cardBase.css`) rides every `srcdoc` ahead of the fragment — widget behavior (no text selection / touch callout / scrolling, border-box) plus the app's design language as tokens (`--ink`/`--muted`/`--line`/`--ok`/`--bad`) and utility classes (`.card`/`.label`/`.hero`/`.row`/`.divider`/`.foot`/`.dot`/`.bar`) — so cards ship structure, not re-authored boilerplate, and stay on-language by default. Overridable by construction: the card's `<style>` comes later in the cascade and the palette is custom properties. Additive-only, like the SDK — installed cards assume it. **Edit mode is entered only from the native header's Edit pill** (`deck.setEditMode` over the bridge; the same pill toggles to Done). Long-press-to-edit was tried and removed: gestures inside an iframe never bubble to the shell document, so a hold over card content — which is the whole tile — either did nothing or was claimed by WebKit's text-selection UI; an in-frame SDK detector worked, but an explicit button is clearer and leaves holds to the card. The shell shows the card's title only as an edit-mode chip — in normal view the fragment owns the full tile including its heading.

The two SDK halves ship in different artifacts on different release trains (gateway binary vs iOS app bundle) — safe, because neither half speaks to the other: the only cross-artifact contract is the card's own `openapi.json` ops and its snapshot JSON. The accepted trade of injected-latest (vs vendoring a pinned SDK copy into each bundle): `spec_hash` covers the agent-written half only, and a gateway upgrade swaps the preamble under every installed card. Two disciplines blunt it: the `ctx`/`deck` API is **additive-only**, and after a gateway upgrade the supervisor **re-runs each card's dry-run** before enabling it, quarantining incompatibilities visibly instead of letting them fail on a timer at 3 a.m. If pin-exactness is ever wanted, the escape hatch is mechanical, not architectural: vendor the current preamble into the bundle at install and point the spawn at it.

### The universal `ctx` — no capability configuration

This section reversed twice during design; the history matters. The first design gated every capability behind an operator allowlist in `baybo.json` (`deck.allowed_hosts`, `deck.allowed_secrets`, …); the second reduced that to declare-equals-granted from the manifest. The final decision goes further: **no capability declaration exists at all.** Every service receives the same `ctx`:

- **`ctx.fetch(url, opts)`** — external HTTP, executed by the Rust parent. The parent applies the SSRF floor — the shared primitive is `baybo_security::is_blocked_ip` (loopback / RFC1918 / link-local / CGNAT / ULA and friends); the URL-validation + safe-resolution pair wrapping it is reimplemented over `is_blocked_ip` in deck's `host.rs` — vet the URL, resolve the host, drop blocked addresses, pin the connection to the vetted ones (`web_fetch.rs`'s own pair stays private to that tool) — then reveals `[{REDACTED_SECRET_…}]` placeholders (note the asymmetric `[{ }]` delimiters: `{{ }}` was explicitly rejected in `placeholder.rs` because every mainstream template engine parses it) from the `SecretVault` into headers at egress, performs TLS, and returns the body. **Secrets remain parent-only** — the `SecretVault` never crosses to the child, so the placeholder reveal stays the *only* path a secret reaches a request, and every such reveal is audited. What the host-run model gives up is enforcement of the SSRF floor for I/O the card initiates *itself*: nothing now stops a card opening its own socket to loopback. The floor still governs `ctx.fetch`; for card-initiated sockets it is advisory, consistent with the trusted-author model below.
- **`ctx.exec(cmd)`** — `/bin/sh -c <cmd>` run **directly on the host** with the inherited environment (installed CLIs and credential dirs resolve), in a per-card scratch cwd, bounded by a 30s wall clock + stdout/stderr size caps. This is what makes "any card the user can describe" true: `docker ps`, `df`, `git log`, `smartctl`, `nvidia-smi`, `codex …` — anything the operator's own shell could run. On top of the inherited env, `host.rs` injects one var: **`BAYBO_DECK_TMUX_DIR`** = `<workspace>/deck/tmux-socks/<card_id>` (`DECK_TMUX_DIR_ENV`, a per-card subdir), the directory a card that drives an interactive CLI through tmux should pin its socket into (`tmux -S "$BAYBO_DECK_TMUX_DIR/<name>.sock"`). It keeps that agent-driven tmux server off the user's default socket (`/tmp/tmux-<uid>/default` — so out of their `tmux ls`) and out of `/tmp` (so a `/tmp` wipe can't drop it), and the per-card scoping is what lets purge reap exactly the departing card's servers; the runtime exports the path but the card `mkdir -p`s it. The `tmux-socks/` tree is gitignored in the deck root (`repo.rs`).
- **`ctx.emit(json)`** — a snapshot push (see data plane).
- **Blob primitives** — `ctx.fetchBlob` (fetch → store) and `ctx.blobPutFile` (an `exec` artifact → store): the file/image plane, ref-first so bytes stay out of the child. See §Blobs.

**Trust model, stated honestly.** There is **no service sandbox** — timeouts and quarantine are *reliability* machinery, not a security boundary against the card author, and the author is the operator's own agent, the same one trusted to run `Bash` on this host. Running the service on the host (full filesystem, host network, host CLIs) is exactly that same trust, made explicit; the removed `crates/sandbox` jail only ever added reliability caps we kept and network isolation the real cards couldn't work under. What makes deck different from chat-driven `Bash` is that services run **unattended, forever**, with no per-call approval gate and no human watching call #4,000; and the author is *steerable* (the agent reads untrusted content, and a prompt-injected card revision is a standing channel rather than a one-shot command). The final design accepts that residual risk explicitly — a prompt-injected card edit can self-grant secret egress with no gate. What still stands: secrets exist only parent-side and reach requests only by placeholder reveal; **every secret-bearing egress is audit-logged with card id + host**; and on the tool path (`DeckCardCreate`/`DeckCardUpdate`) the dry-run gate means new code executes once under the agent's eyes — with its output in the transcript — before it runs unattended. The non-tool dry-runs (post-upgrade boot re-runs, REST/FFI-triggered enable) run the same gate with no transcript; their failures surface as trace events plus the quarantine face, nowhere else. (Restore deliberately does NOT gate — see the recycle-bin section.) (The design's advisory install pre-screen via the skills risk assessor did not land — nothing scans a bundle for exfil shapes.)

### Tick ownership and emit policing

The **service owns its own clock** (a `setInterval` in `start(ctx)`, per the manifest's declared cadence) — chosen over a gateway-driven ticker for cadence flexibility (irregular schedules, event-driven updates, provider-429 backoff). The forced consequence: the gateway cannot withhold a tick, so it polices **ingestion** instead. `emit` is clamped to a per-card minimum accepted interval — the manifest's declared `min_emit_interval_secs`, floored by a gateway-wide named const (1s) — with excess emits coalesced to latest; `seq` is assigned by the gateway; snapshot size is capped; and emit floods count toward quarantine. Pause and quarantine are implemented by **stopping the process** — untrusted code is never trusted to rate-limit itself.

## The admission contract: per-card OpenAPI

Each service publishes its op surface as its own `openapi.json`, and the spec is **load-bearing, not documentation**: at install the gateway parses it into compiled per-op validators, and every call that **crosses the gateway** — user-initiated ops and every dry-run invocation (install, update, re-admission) — is validated *before* anything reaches agent-written code (op exists, params match, unknown fields rejected, body size capped). The document wrapper stays a hand-checked narrow subset (op-name grammar, `get`/`post` only, mandatory `x-baybo-retryable`) but the parameter check itself is **standard JSON Schema compiled by the `jsonschema` crate** — each declared parameter's `schema` is taken verbatim into a per-op `{type: object, properties, required, additionalProperties: false}` validator, so scalars, enums, and typed `array`/`object` params with nested constraints all validate. Two hard edges: `$ref`/`$dynamicRef` are rejected at admission, and the crate is built `default-features = false` — its default remote resolvers (HTTP + file) are compiled out, so an agent-written schema can never make spec compilation touch the network or filesystem. Every op also carries a **mandatory `x-baybo-retryable` boolean** — the author's declaration that replaying the op is harmless (pure read/recompute) or not (side effects); install refuses a spec that omits it, so replay-safety is always a decision, never a default. The `true` set rides the deck view (`retryable_ops` per card) to clients, whose transports use it as the per-op replay verdict. Off-schema requests die at the gateway. Be precise about the boundary: the self-timer's own op invocations are plain in-process function calls inside the service and never cross the gateway, and runtime `emit` payloads are policed for rate and size only — the snapshot is vetted (non-null JSON, size-capped) at gate time, not per tick. The spec is also served (`GET /v1/deck/services/{uuid}/openapi.json`) so the agent writing `card.html` and future tooling can introspect a card.

Per-card ops can **never** merge into the main `docs/openapi.json` — that document is snapshot-tested byte-for-byte (`openapi_spec_sync.rs`) and describes the static surface. The deck's own management routes join it normally (`UPDATE_OPENAPI=1` regen); the per-card ops are a deliberately separate dynamic surface addressed by UUID.

Op semantics are **unrestricted on the host** — no read/mutate taxonomy. The card's power surface is exactly `ctx`, and the SSRF floor keeps the gateway's own REST unreachable from `ctx.fetch`, so "card mutates the gateway" is dead by default without an effect system.

## Data plane

Card JS has **no active network** — no fetch, no XHR, no WebSocket, enforced by CSP and the iframe sandbox (below). (The one passive affordance is a read-only `<img>` over the local `baybo-transcript:` blob route — §Blobs — served by the app's own scheme handler, never a real socket.) This was forced, not chosen: in relay mode the gateway has no reachable URL at all (the phone reaches it only through the Noise tunnel inside the FFI), so a card that literally `fetch()`ed a gateway endpoint could never work for relay users, and no scoped-token story exists for the direct leg (the phone's stored direct-mode credential *is* the admin token). Everything crosses the native bridge.

**Push is the primary plane.** A service tick flows:

```
service ctx.emit → gateway: policed, seq-stamped, stored in deck_snapshots
  → Frame::DeckCardData { card_id, seq, payload } broadcast on the owner channel
  → iOS FFI dispatch_inbound_frame (new arm, BEFORE per-session routing)
  → DeckSink (connection-global, the SessionListSink pattern)
  → Swift DeckStore → bridge → shell → iframe postMessage → deck.onData
```

The connection-global sink is mandatory, not optional: today any unrecognized session-less frame is fanned to per-session chat `FrameSink`s only, so a user parked on the Deck tab with no chat subscribed would receive nothing.

Broadcast scope: the owner channel is shared — the web dashboard registers on it too (the `SessionPulse` precedent) — so deck frames land on `app/web` WS connections whose deck parity is deferred; the web client must ignore them (its `wireSentinel` update is exactly this obligation). The TUI is not an owner-channel client and never sees them. One interaction to note: a slow owner-channel consumer whose queue fills gets nudged with `Frame::Gap{session_id: None}` (full resync), so a card emitting at its clamp floor adds pressure on clients that cannot even render it — the floor const is now 1s (lowered for live/game cards), so this leans on coalescing-to-latest plus the web client dropping deck frames to bound the pressure. `Frame::DeckChanged` (no payload) signals structural change — install, update, delete, restore, purge, enable, disable, quarantine, layout — and clients respond by refetching `GET /v1/deck`.

**Pull covers open and taps.** `GET /v1/deck` returns cards + layout + latest snapshots + seqs (instant paint from cache). `seq` is a **persisted per-card monotonic counter on the `deck_cards` row** — never derived from the prunable snapshot table, so it cannot regress across a gateway restart (the sentinel/cursor bug class `docs/sync-protocol.md` exists to kill). Client rule: accept a push iff `push.seq >` the cached seq; a `DeckChanged`-triggered refetch replaces cached snapshot + seq unconditionally. User-initiated card interaction (`deck.call`) travels shell → bridge → FFI → `POST /v1/deck/services/{uuid}/{op}` → spec validation → stdio `call` → JSON back. Same validated surface as the scheduled path.

### REST surface

All under `require_admin_token` on the admin router; the relay API tunnel admits any `/v1/*` path, so the whole surface works over both legs with zero extra transport work.

| Route | Purpose |
|---|---|
| `GET /v1/deck` | cards + layout + latest snapshots + seqs |
| `PUT /v1/deck/layout` | full ordered layout write `[{uuid, position, size}]` |
| `GET /v1/deck/recycle` | soft-deleted cards, most recent first |
| `POST /v1/deck/cards/{uuid}/enable` / `disable` | run-state toggle (disable stops the process) |
| `DELETE /v1/deck/cards/{uuid}` | soft delete (below) |
| `POST /v1/deck/cards/{uuid}/restore` | recycle-bin restore |
| `POST /v1/deck/cards/{uuid}/purge` | hard delete out of the bin (row, snapshots, bundle files, runtime residue, `deck:*` blobs) |
| `GET /v1/deck/cards/{uuid}/bundle` | `card.html` (reaches the shell via `deck_fetch_bundle`, below) |
| `GET /v1/deck/services/{uuid}/openapi.json` | the card's op contract |
| `POST /v1/deck/services/{uuid}/{op}` | validated on-demand op call |

### Wire and contract obligations

Two new frames (`DeckCardData`, `DeckChanged`) mean: ts-rs regen (`scripts/check-ts-bindings.sh`), updates to both hand-written `wireSentinel.ts` mirrors (`app/ios/web`, `app/web`), a new UniFFI callback interface (`DeckSink { on_card_data, on_deck_changed }`) + Swift bindings regen (`build-core.sh`). Both legs skip undecodable inbound frames — `DirectCodec` (`direct/chat.rs`) and the relay `ContentSession::open` (`core/content.rs`, which explicitly mirrors the direct codec's forward-compat posture) — so older clients tolerate the new frames; deck makes that skip behavior load-bearing on both legs.

New `BayboClient` FFI methods, each routed over the active leg like every `chat_*` call: `deck_fetch`, `deck_fetch_recycle`, `deck_fetch_bundle(uuid)` (the shell holds no gateway URL or token in either mode — `card.html` reaches it over the bridge like all other data), `deck_call(uuid, op, params_json)`, `deck_set_layout`, `deck_set_enabled`, `deck_delete`, `deck_restore`, `set_deck_sink`.

## Storage

Follows the `SessionFolderStore` recipe: `DeckCardStore` trait + row DTOs in `crates/store`, sqlite impl in `crates/storage/src/sqlite/deck.rs`, tables in `init_db`'s CREATE batch (new tables need no migration entry), a new `Arc<dyn DeckCardStore>` field on the `Store` DI bundle. (`DeckCardStore`, not `DeckStore` — the Swift client-side observable is already named `DeckStore`, and one grep should never conflate the persistence port with the UI store.)

- **`deck_cards`** — `id` (uuid PK), `title`, `position`, `size` (`small` 1×1 / `wide` 2×1 / `large` 2×2), `sizes` (comma-joined implemented set; empty ⇒ `[size]` for a legacy row), `maximize` (declares a `"max"` layout), `enabled`, `quarantined_at`, `deleted_at`, `spec_hash`, `last_seq` (the per-card monotonic push counter), `created_at`.
- **`deck_snapshots`** — `card_id`, `seq`, `payload`, `fetched_at`, `error` (latest row per card is the paint source; ephemeral render state, not transcript). Retention is **prune-on-insert in the sqlite impl** — a plain `DELETE` keeping a named-const latest-N per card, as `storage.md` sanctions; the janitor's charter ("no storage compaction") stays untouched and no background sweeper is involved.

**Deletion is a soft-delete recycle bin, mirroring `cron_jobs`** (the one precedent for "user-authored, painful to recreate"): delete stamps `deleted_at`, stops and deregisters the service, hides the card, and keeps `workspace/deck/<uuid>/` intact; listings apply the live-only predicate in SQL. Restore clears the stamp and starts the service **without re-running the dry-run gate** — a deliberate walk-back of the original "every transition into the fleet re-gates" rule. The bundle is byte-identical to when the user deleted it, the user is present and watching (an unbootable restore hits the supervisor's crash→quarantine within seconds, visibly), and the quarantine's Re-enable path IS still gated — `enable` remains the re-admission verdict, refreshing the error face on failure. Gating restore bought little beyond seconds of latency and a redundant op execution per restore. The gate stays where new code enters (install/update), where drift is silent (post-upgrade boot re-gate), and where a health verdict is the point (enable). The restored card lands with its last pre-delete snapshot (soft delete keeps `deck_snapshots`); the resident service's first tick refreshes it. Restore counts against the card cap like an install. Purging from the bin is the hard delete that removes files — the bundle dir plus the card's runtime residue: any tmux server still behind a `*.sock` in the card's `tmux-socks/<card_id>` dir is best-effort `kill-server`ed, then that socket dir and the card's `state/deck-scratch/<card_id>` exec scratch are removed (missing residue is ignored; residue cleanup never fails a purge). Purge also reclaims the card's `deck:<card_id>` blobs (§Blobs GC) — the only time deck blobs are swept.

**Scratch lifecycle.** `ctx.exec` runs in a per-card cwd under `state/deck-scratch/<card_id>`, reaped at purge (above) — purge reaps runtime residue *before* the fallible bundle-dir removal, so an fs error can't strand a live tmux server behind an already-purged row. The dry-run gate is hermetic: its throwaway service runs as `gate-<uuid>` with its own scratch dir *and* its own `tmux-socks/gate-<uuid>` socket dir, and when the gate finishes — pass or fail — the same runtime reap purge uses kills any tmux server the gate's execs pinned there and removes both dirs, so a gate's tmux servers and sockets die with the gate and a dry run never touches the resident card's server. Crash residue is caught by `DeckManager::boot`'s orphan sweep: every entry under `tmux-socks/` or the scratch root that is `gate-*` or belongs to no existing card row (live and soft-deleted rows both count — a recycled card keeps its runtime residue for restore) has its `*.sock` servers killed and its dir removed. The sweep only touches entries older than one hour (`boot()` runs in a spawned task racing the live server, so a just-started gate's fresh dirs must survive); the reap skips `.sock` symlinks (a planted link must not aim `kill-server` at the user's own socket) and bounds each `kill-server` at 5s.

**Job/Trace boundary (explicit deviation).** The repo constraint says background execution enters Job and Trace. Deck services are streaming residents, not discrete work items: modelling every 5-minute tick as a Job would flood both stores with noise. The decision: **card provenance and lifecycle transitions** are recorded as trace events — install, update (`spec_hash` before → after), delete, restore, purge, start, stop, crash, quarantine — satisfying the "hot reload and tool updates leave provenance records in Trace" constraint; individual ticks and op calls are not Jobs. Ops appear in the audit log when they egress with secrets.

**Governance boundary (second explicit deviation).** The repo constraint says tool/skill extensions carry source, version, hash, trust level, and capability declarations. A deck card carries `spec_hash` and an implicit source (the operator's own agent wrote it) — no version field, no trust tier, and capability declarations deliberately abolished by the trust decision above; `ExtensionManifest`/`TrustLevel` in `crates/model` go unused here. This is a knowing deviation, accepted with the trust model. If deck cards ever gain third-party provenance (sharing, import), the governance vocabulary must be adopted before that feature ships.

## Blobs: files and images

The rest of the deck protocol is text-only — `ctx.fetch` returns a
`from_utf8_lossy`'d body and the one byte path into a card is base64 inside a
≤5 MiB snapshot (before this plane the card CSP also admitted only `data:`
images). This plane adds binary
by **reusing the chat blob store wholesale**: `BlobStore` (capability id
`blob_id = sha256:<64hex>.<32hex read-token>`, file-level dedup), gateway
`POST/GET /v1/blobs`, the iOS FFI `blob_upload_bytes` / `blob_download_bytes` +
the content-addressed device cache, and the relay `LegClass::Blob` legs.
Possession of a `blob_id` is the read capability; a card gets *refs*, never bytes
over its port, and never a way into chat attachments (nobody hands it those ids).
The shared machinery is the only thing shared — no cross-visibility, no new ACL
surface. `MAX_BLOB_BYTES` (100 MiB) lives in `baybo-store::blob` as the single
shared cap.

A card does four things with blobs: **display** an image, let the user **upload**
a photo, **share** a produced file back to the user, and — service-side —
**fetch/store/read** bytes ref-first.

### Service `ctx` — ref-first (bytes stay off bun's stdio where they can)

`DeckHost` holds an `Arc<dyn BlobStore>`. Five NDJSON-RPC primitives, keeping
bytes off the child's stdio wherever they can:

- **`ctx.fetchBlob(url, opts) → {blobId, contentType, size}`** — fetches an
  external URL and streams the response **straight into the store**
  (`put_stream`), so an image of any size never enters bun. Same
  `[{REDACTED_SECRET_…}]` reveal as `ctx.fetch`. It uses its own client with a
  connect + idle-read timeout rather than `ctx.fetch`'s 60 s total cap (which
  would throttle a 100 MiB pull to ~1.7 MiB/s), follows redirects manually
  (≤5 hops, re-vetting scheme + blocked-IP/DNS per hop — `Policy::none` keeps the
  address pinning) and **strips credential-bearing headers on a cross-host hop**
  (`Authorization`/`Cookie`/`Proxy-Authorization` plus any header a secret was
  revealed into — the manual follow bypasses reqwest's own cross-origin
  stripping), and stores only a 2xx body. Inside an op it is still bounded by the
  30 s `CALL_TIMEOUT`; large pulls belong in `start()` / timer contexts.
- **`ctx.blobPutFile(path, contentType) → ref`** — streams an `exec`-produced
  file into the store. Relative paths resolve against the card's exec scratch
  cwd; absolute is allowed (trusted author); a `..` climb out of scratch is
  rejected as a footgun. This is the only *put* path: a service that generates
  content writes it to disk in an `exec`, then hands over the file; small
  display-only content can instead ride a `data:` URI in the snapshot (no blob).

Every service-side put stamps `uploader_identity = "deck:<card_id>"` so GC can
target deck blobs without ever touching chat data. The identity is threaded
separately from the process id (`SpawnConfig.uploader_card_id`): the two are
equal for a live service, but the **dry-run gate** runs under a throwaway
`gate-<uuid>` process id while its blobs must carry the card's *eventual* real id
— so `install` mints the card uuid before the gate, and update / enable / boot
pass the known id.

### Display — the `baybo-transcript:` scheme

The card CSP admits `img-src baybo-transcript:`, and `sdkCard.js` exposes a
synchronous `deck.blobUrl(ref | id, ct?)` that composes
`baybo-transcript://localhost/blob/<blob_id>?ct=<mime>` (id raw in the path, mime
canonically encoded — WebKit keys its memory cache on the full URL). Point an
`<img>` at it. The app's `TranscriptSchemeHandler` (deck webview only — a
`blobRouteEnabled` init flag) serves the `/blob/` route ahead of the bundle-file
fallthrough. The serve logic lives in **one FFI call** —
`blob_bytes_for_display(blob_id, max_bytes) → BlobServeOutcome` — so the handler
stays a thin WebKit adapter. The core validates the id shape, reads the
content-addressed device cache **cache-first** (no network, no binding — a cached
image renders offline / unbound), downloads leg-bound on a miss, and enforces the
display cap (an over-cap *cached* blob is refused by its stat, never read; an
over-cap *downloaded* blob is materialized once, then refused). Swift maps the
outcome (`Bytes` / `OverCap` / `NotFound` / `BadId`) to the `WKURLSchemeTask`
response, filling `Content-Type` from `?ct=` and `Content-Length`. That 8 MiB cap
is a hard **display** ceiling: a service can store up to `MAX_BLOB_BYTES`
(100 MiB) via `fetchBlob` / `blobPutFile`, but only a blob ≤8 MiB is displayable —
a larger one stores fine and the `<img>` just fails.

That this WebKit path works at all was not answerable from code — whether a
sandboxed opaque-origin `srcdoc` iframe's `<img>` subresource reaches the
*parent* webview's `WKURLSchemeHandler`, and whether a meta-delivered CSP admits
a custom scheme inside that frame — so it was settled with a throwaway simulator
probe before the `deck.blobUrl` contract was frozen (both YES; the async
`deck.blobData`-over-the-port fallback was never needed). The probe is gone —
every real card image now exercises the same srcdoc → scheme-handler → CSP chain,
so a future WebKit regression surfaces in normal use.

Two handler invariants. The async FFI-backed serve **tracks live
`WKURLSchemeTask` identities** and confirms one is still live before every
`didReceive` / `didFinish` / `didFail` — WebKit tears a task down when the card
is resized/removed mid-load, and messaging a stopped task crashes. And a failed
load is terminal for that `<img>` until the tile rebuilds, so a card that must
recover handles `onerror` by re-setting `src`.

**Accepted limitation — the on-device cache read is digest-keyed, not
token-gated.** The device cache (shared with chat) is content-addressed by the
64-hex digest; the `/blob/` route reads it by digest and ignores the read token
(only the gateway download leg, on a cache miss, checks it). So a card that
*knows* a digest can display any already-cached blob — a chat attachment,
another card's — with a fabricated token. This is inert, not a leak: the digest
is itself a 256-bit unguessable capability a card can't learn (no chat access,
no enumeration, `default-src 'none'`), and a displayed blob has **no
exfiltration path** — it renders in the card's opaque-origin, no-network iframe,
and the only outbound channels (`deck.call` to the card's own service,
`deck.log`, user-mediated `deck.shareBlob`) can't ship the bytes anywhere
untrusted. Scoping the handler to deck-delivered ids is the defense-in-depth fix
if one is ever wanted; making the *cache* honor the full id is the wrong lever
(it breaks the content-addressed dedup shared with chat).

### Upload — native picker, ref over the port

`deck.pickBlob({accept}) → Promise<{blobId, contentType, size, name?}>` runs
card → shell → bridge → a native SwiftUI `PhotosPicker` (photo library) →
`blobUploadBytes` → the ref resolves back. Bytes never cross the port; the card
passes the `blobId` as a plain string into a following `deck.call` for its
service to consume, and/or into `deck.blobUrl` to display it. The mime comes from
`supportedContentTypes.first?.preferredMIMEType`, the byte count is pre-checked
against the shared 100 MiB cap, and `name` is synthesized from the mime
(`PhotosPicker` exposes none).

Two contracts the UI enforces. **One pick at a time**: a single `activePick`
slot, held from selection all the way through the upload — a concurrent
`pickBlob` gets an immediate typed `busy` (never a queued picker popping up
seconds later as a ghost dialog), a dismissal with no selection resolves
`cancelled`, and every request settles exactly once. **The shell's call/pick
correlation is generation-guarded**: each pending entry pins the `MessagePort`
it was made on, a result is delivered only to that same tile generation, and an
iframe reload/removal purges the card's pending entries — otherwise a spec-change
reload (whose minutes-long `pickBlob` window makes it routine) would resolve the
wrong promise and then silently eat the new generation's real answer.

A picker upload is stamped `deck:<card_id>`, the same identity a card's service
produces — so it is **reclaimed at purge** alongside the card's other blobs. The
device sends the card id on an `x-baybo-deck-card` header (`deck_blob_upload_bytes`
FFI); the gateway's device-upload arm honors it (`device_upload_identity` in
`crates/gateway/src/channel/blobs.rs`) in place of the plain `device:<id>` marker.
The card id is unforgeable by card JS (it comes from the shell's port→card map,
threaded as `DeckStore.activePick.cardId`); the gateway does not validate the
card exists, since the identity is a GC/diagnostic marker, never an access gate.

### Share

`deck.shareBlob(ref | id, {filename})` is fire-and-forget: native fetches the
bytes (cache-first), materializes them on disk under a real filename
(`<tmp>/baybo-deck-share/<digest>/<name>`), and presents `UIActivityViewController`
via the `ChatStore.fileShare` / `FilePreview` sheet idiom on `DeckStore` — so
Save-to-Photos / Files / AirDrop keep the original encoding.

### GC — purge-time reclamation

Deck blobs are reclaimed only when a card is **purged** from the recycle bin.
Otherwise they persist, like chat attachments — the shared cache "only grows,
deliberately." Purge deletes the card's `deck:<card_id>` blobs
(`list_ids_by_uploader`, an indexed range scan on `uploader_identity`, so chat
blobs are never in range); it runs after the row + snapshots are gone, and
`delete()`'s own `any_live_for_path` (an indexed PK-range check on the digest,
not a full-table scan) still spares a content file shared with another live blob.
This covers both the service's own blobs and images a user uploaded through the
card's picker — the gateway stamps both `deck:<card_id>` (see §Upload).

There is **no timed sweep**. A machine-paced card that fetches a *new* image
every tick — rather than reusing the blob id for unchanged content, the SDK's
reuse rule — accretes blobs until purge; that's accepted, the same only-grows
posture chat already has. A conservative TTL sweep keyed on snapshot-reference
liveness was built and removed as disproportionate for a single-operator machine
(and `last_accessed_at`, the usual LRU signal, is useless here: the
content-addressed device cache never re-fetches, so a blob a card renders daily
never bumps the server clock).

### Versioning and deferred

`SDK_VERSION` **stays 1**. The `ctx` blob additions are additive — an old card
never calls them and runs unchanged against the always-fresh (`include_str!`d)
preamble — so no fleet re-gate is forced. Bumping it would only force a one-time
boot re-verification of every card (and risk quarantining a network card on an
offline boot for no gain); a future *breaking* preamble change is what would
earn the bump. **No wire-frame changes** — refs ride snapshot / call-result JSON
opaquely, `SNAPSHOT_MAX_BYTES` is unaffected.

Deferred: in-card video/audio playback (`media-src` + range-request scheme
serving, which forces chunked `didReceive`); in-card fullscreen preview
(ImageViewer / QuickLook); a streaming `blob_upload_file(path)` FFI so a large
pick isn't read into memory before upload; arbitrary-file picking (`fileImporter`,
not just the photo library); bounding a cache-*miss* display download before it
materializes; web deck parity for blobs.

## iOS client

### Tab and navigation

`HomeTab.agents` is renamed to `.deck` outright — label Deck (en + zh-Hans), icon `rectangle.stack` — including the `-baybo-home-tab` debug-arg literal and the `-baybo-demo-tabs` cycle order (the "agents" placeholder was never real, so no compat shim). `PlaceholderScreen` survives for the Projects tab. `app/ios/docs/navigation.md`'s navigation prose changes accordingly.

### Render host: one deck webview, per-card iframes

The deck is the app's **second** WKWebView (`DeckHost`, mirroring `TranscriptHost`: own scheme-handler instance serving the deck shell bundle, prewarmed once a binding reaches home, kept warm until logout/rebind). The shell — trusted code, a second Vite entry in the existing `app/ios/web` workspace (no shared-package seam; web parity is explicitly deferred, and a future `app/web` deck page is an extraction refactor) — renders the grid and hosts one iframe per card:

- `<iframe sandbox="allow-scripts" srcdoc="…">` — **no `allow-same-origin`**, so each card gets a unique opaque origin: no storage, no cookies, no parent DOM, no access to the shell's native bridge.
- The shell injects a **CSP meta** into every `srcdoc` (`default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: baybo-transcript:`) because the sandbox attribute alone does **not** block network — an opaque-origin frame can still fire-and-forget a cross-origin POST. CSP closes that. The one network affordance is `img-src baybo-transcript:`, which admits an `<img>` over the local custom-scheme blob route (§Blobs) — served by the app's own `WKURLSchemeHandler`, never a real socket; `script-src`/`connect-src` gain nothing, so fetch/XHR/WebSocket stay blocked.
- **Card identity is a per-card `MessagePort`**, minted by the shell and transferred into the iframe once at init. With `srcdoc` sandboxing, every card's `postMessage` arrives with `event.origin === "null"` — identity by claimed uuid would let card A read card B's data. Port identity is bound shell-side — each port's message handler closes over its card's uuid, which the card can neither see nor forge; closing the port mutes a removed card instantly.

This keeps the app's web-content process count at 2 (transcript + deck) regardless of card count, at the cost of web-implemented drag gestures. Those gestures are a **small custom engine with iOS-home-screen mechanics** (SortableJS was tried and removed — its clone-fallback model can never paint an iframe, so the finger dragged a blank box). The engine is built around one constraint: an `<iframe>` reloads whenever its node moves in the DOM, but `transform` and CSS `order` never touch the tree. So the REAL tile lifts (`position: fixed` + translate — its document keeps painting live), a plain-div placeholder holds the slot, neighbors FLIP-slide via WAAPI, tile placement is CSS `order` (tiles are appended once and never reparented), and the drop is pure style — zero reloads at any point. Two WKWebView traps are load-bearing here: the native UIScrollView pan claims the touch ~2 moves in and fires `pointercancel` unless the tile does a **non-passive `touchmove` preventDefault** while editing (`touch-action: none` alone does NOT hold once the mid-gesture `position:fixed` flip triggers re-arbitration), and the drop must re-resolve its slot from the final pointer position because the last move can still be waiting on rAF. Edit mode itself is entered from the native header's Edit pill.

One WebKit runtime behavior was not answerable from this repo's code — CSP-meta + `srcdoc` subresource semantics inside a custom-scheme-hosted page (`baybo-transcript://`) — and was settled by a throwaway simulator probe when the blob display path landed: a sandboxed opaque-origin `srcdoc` iframe's `<img>` subresource DOES reach the parent webview's `WKURLSchemeHandler`, and the meta CSP's `img-src baybo-transcript:` admits it (§Blobs).

### Layout: ordered flow + size classes

"Free-form" resolved to **order + size**, not x/y placement: each card holds a `position` index and a size class (Small 1×1, Wide 2×1, Large 2×2) on a 2-column grid that reflows automatically — the iOS home-screen-widget idiom, robust across widths, and drag-reorder in a flow is tractable on touch where free-canvas collision/compaction is not.

Layout lives **server-side** (`deck_cards.position`/`size`) with the `SessionIndex` mutation idiom, deck-shaped: drag applies locally at once, `PUT /v1/deck/layout`; on failure it rolls back to the server-acked **baseline** (never the negation) and refetches — the write is one absolute layout, so no pending-mutation epochs or merge guards are needed. A small local mirror (`Application Support/baybo/deck.json`) gives instant offline paint.

### Install UX

Creation is live: `DeckChanged` fires only after the dry-run gate has stored the first snapshot, so the card arrives on the deck **already populated** — the refetch it triggers carries the snapshot, and any loading face lasts only the iframe-boot instant — typically before the agent's chat reply lands. The reply says so in words — the skill has the agent tell the user the card is on their Deck; there is no deep link (the tab is one tap away). No modal and no per-card phone confirmation, consistent with the trust model.

### Client build, localization, CI

The tab label has `Localizable.xcstrings` keys (en `Deck` + the zh-Hans kanban term), plus the deck-native strings (`deck.editDone`, the delete-confirm set); the shell carries its own tiny en/zh string table fed by a `setLanguage` bridge call (deliberately not the transcript's i18next — the shell is dependency-free vanilla TS). Packaging: the shell is a second entry in the SAME dist, so `build-app.sh`'s existing `web/dist → App/Resources/transcript/` copy ships it with no new step and `project.yml` is untouched. CI: no filter changes — the shell lives under the already-filtered `app/ios/web`, and `crates/deck` is not an ffi dependency (the iOS jobs are currently disabled anyway, so the tiers run by hand and the PR body says so).

## Authoring pipeline

Card authoring is **explicit-invocation-only and owner-channel-only**: the
builtin `deck` skill is slash-only (`command: deck` +
`disable-model-invocation: true`) and `channels: [owner]`, and all four
`DeckCard*` tool manifests (`List`/`Get`/`Create`/`Update`) carry
`channels: [owner]` — so the model never volunteers a card, a
telegram/tui/subagent session neither sees the skill nor the tools (they're
filtered from its LLM tool list and refused by the executor), and a card is
only ever authored when the owner types `/deck <request>` in chat (web or
iOS; it's a plain chat message, no client plumbing).

When the user types `/deck`:

1. **Skill.** The slash expansion injects the builtin `deck` skill: the bundle contract, the `ctx`/`deck` SDK surface, and worked examples deliberately spanning genres (an API-fetch quota card *and* a fetch-free machine-status card via `ctx.exec`) so the skill doesn't anchor generation to one shape.
2. **Staging.** The agent scaffolds and edits the four files in a scratch dir with its ordinary `Write`/`Edit` tools — the same loop it uses for all code.
3. **`DeckCardCreate(path)` — install is a dry-run gate.** The tool: (a) static-validates (manifest + spec parse, size caps, refresh op declared with on-schema params); (b) boots the service on the host — a missing `ops` export dies here; (c) invokes the refresh op once; (d) rejects a null or oversize snapshot — all **before** the row enables or `DeckChanged` fires. Failures (stderr, bad JSON, timeout) return in the tool result so the agent iterates in the same turn; success stores the first snapshot, so the user's first sight of the card is populated, not a spinner.
4. **Preview (best-effort).** When the browser sidecar is present, the tool renders `card.html` with the first snapshot and attaches a screenshot to the chat reply; absent a browser it degrades to the plain dry-run silently.

Updates re-run the same gate; `DeckCardUpdate(card_id, path)` replaces the bundle and restarts the service.

**Cross-chat updates need discovery, not memory.** A card persists across conversations, but a *new* chat has no memory of its uuid and the agent's file tools are sandboxed to `workspace/work/` — they can't reach `workspace/deck/<uuid>/` to read the installed source. So the authoring skill carries two read-only discovery tools alongside the authoring pair: **`DeckCardList`** (every live card's `card_id`/`title`/`size`/`sizes`/`maximize`/`enabled`/`spec_hash`, so the agent resolves "the quota card" → a uuid) and **`DeckCardGet(card_id)`** (the card's current source returned *inline* in the tool result — the four bundle files plus any `src/` pre-build files keyed by their `src/…` path — so the agent edits from the real source rather than re-authoring blind; the sandbox-reach problem is why the source comes back through the tool result, not a path). The update flow is then: `DeckCardList` → `DeckCardGet` → write the files to a fresh scratch dir → surgical edit (re-`bun build` if the card has a `src/`) → `DeckCardUpdate`. All four deck tools are `Trusted` + `channels: [owner]`; the authoring pair holds `ReadFile` (they read the agent's staged dir), the discovery pair holds no filesystem capability (they read through the manager). Without these, updating in a new chat would force a full re-write from the user's description, silently dropping whatever the card already did.

The four tool **descriptions are deliberately terse** — one line each, pointing at the `/deck` skill. The full bundle contract (the four-file shapes, the `ctx`/`deck` SDK, worked examples) lives only in the skill, which is injected on `/deck` and nowhere else; the tool *schemas*, by contrast, ride every owner-channel prompt in the cached prefix whether or not the turn touches the deck. Keeping the contract out of the always-on descriptions holds the four tools to ~326 tokens (from ~621 before the trim) with no prompt-cache disruption — the model still sees the whole contract exactly when it authors, via the skill.

## Security hazards & limitations

"Trust model, stated honestly" (§The universal `ctx`) is the governing frame: the card author is the operator's own agent, trusted like `Bash`, so the threat model is **not "a malicious card."** It is the four residual surfaces that author-trust does not cover — (1) a prompt-injected card edit is a standing self-grant channel; (2) `ctx.fetch` talks to an **untrusted remote**, which author-trust does not extend to; (3) resource exhaustion against a **single shared-process gateway**; (4) the phone-side frontend runs **arbitrary agent HTML**. What follows are the residual hazards and the deliberate non-guarantees, each back-referencing a mechanism defined above rather than restating it. Commit `4e8ba066` (caps loosened for game-scale cards) widened several — flagged inline.

### Availability — the gateway is one shared process

- **Unbounded `ctx.fetch` body → gateway OOM (highest-impact).** `fetch_external` is host-mediated: the *parent* gateway buffers the whole upstream response into an in-memory `Vec<u8>` with **no size cap** (the cap was deleted in `4e8ba066`; the `resp.chunk()` loop under "No response-body cap by design" in `host.rs`), then copies it again via `from_utf8_lossy` (up to ~3× on binary) and re-serializes it to the child. `FETCH_TIMEOUT` (60s) bounds duration, not bytes — at real bandwidth a card pointed at one large/endless URL pulls multiple GB into gateway RAM and OOM-kills the process that hosts *every* session, channel, and the other 63 cards. Author-trust does not contain this (the remote is not the author), and quarantine never sees a single fatal allocation. `SNAPSHOT_MAX_BYTES` gives false assurance here — it bounds what the child hands back, not the parent's fetch buffer. Cheapest fix: restore a streamed byte cap in `fetch_external`.
- **Unbounded concurrent host RPCs.** The reader pump in `spawn_service` (`service.rs`) `tokio::spawn`s every child `exec` / `fetch` / blob-RPC request line with no semaphore or in-flight budget; each `exec` spawns a real `/bin/sh -c` buffering up to 8 MB (4 MB stdout + 4 MB stderr) for 30s. A tight loop is a fork/thread/RAM storm, and quarantine is structurally blind — strikes count only crashes, call-timeouts, and emit floods, never fast-returning execs.
- **No aggregate memory/concurrency budget.** The only fleet-wide bound is `MAX_CARDS` (64); nothing caps resident-service memory, in-flight RPCs, concurrent fetch buffers, or broadcast backlog, so the per-item caps compose with no ceiling. Post-`4e8ba066`: a 5 MB snapshot is **cloned per owner connection** at fanout and each connection's frame queue is 64-deep → up to **64×5 MB = 320 MB backlog per stalled connection** (20× the old 16 MB), fillable ~10× faster at the 1 s floor; `GET /v1/deck` materializes up to ~320 MB of latest snapshots per in-flight request and every `DeckChanged` tells all clients to refetch (thundering herd); `deck_snapshots` holds ~64×3×5 MB ≈ **960 MB** steady-state in the *primary* sqlite DB (≈53× pre-loosening), taxing DB size, page cache, and backups. A card emitting near-cap snapshots at 1 Hz with completing fetches is "well-behaved" by every metric quarantine measures — the per-card egress budget the design **declined** (see below) is materially more consequential now than when it was declined.

### Backend — no inter-card isolation

The frontend is well-isolated (opaque-origin iframe + per-card MessagePort, §iOS client); the **backend is not**. Every card's bun service runs as the same host user with full filesystem access, so card A's service can read or overwrite card B's bundle (`workspace/deck/<B>/service.js`), B's scratch dir, the `deck_snapshots` DB, and any host credential directory — a compromised/injected card is contained on the phone but not on the host. Two integrity gaps compound it: **`service.js` is not hash-pinned at (re)spawn** (only `openapi.json` drift is caught via `spec_hash`), so a host-side actor — or another unisolated card — that rewrites a resident card's `service.js` changes its behavior with no gate; and **the four required files follow symlinks** (`std::fs::copy` in `materialize`) while `src/` explicitly refuses them (`copy_src_tree`), so a staged `service.js` symlinked at a host file lands as real bytes and `DeckCardGet` returns them verbatim into the transcript — a half-enforced anti-smuggle boundary.

### Audit & egress are narrower than they read

The "every secret-bearing egress is audit-logged" guarantee is real but narrow, and the surrounding non-guarantees are unstated:

- **The audit fires only on placeholder reveal** (the `revealed`-gated "secret-bearing egress" `tracing::info!` in `fetch_external`, and the twin in `run_fetch_blob` for `ctx.fetchBlob`) — a `ctx.fetch` / `ctx.fetchBlob` carrying no vault placeholder (e.g. `POST` of stolen `~/.codex` creds to `attacker.com`) is never logged, and **`ctx.exec` has no audit hook at all**. The line is host-only, secret-blind, and path-blind, so query-string exfil to a benign-looking host is indistinguishable from legitimate use. (`ctx.fetchBlob` follows redirects host-side — it strips credential headers on a cross-host hop, but its egress is still only reveal-audited.)
- **Env secrets bypass the reveal story entirely.** Deck services and `ctx.exec` inherit the gateway's full environment (no `env_clear`), unlike the channel sidecars, which scrub to an allowlist (and carry a regression test calling the inheriting behavior a leak). Any secret the operator exports into the gateway env (LLM keys, cloud creds) is cleartext-readable via `process.env` by every card with zero audit — the "secrets stay parent-only" invariant is scoped to *vault placeholders*, not env vars.
- **The SSRF floor is not containment.** It governs only the `ctx.fetch` convenience path; a card opens its own socket (`Bun.connect`) or `ctx.exec('curl …')` to reach any address, with no per-card egress policy behind it. (Minor floor nit: `is_blocked_ip`'s v6 path doesn't classify IPv4-compatible `::127.0.0.1` or NAT64 `64:ff9b::/96`.)
- **Snapshot / op-param redaction at rest is unmapped.** A card can emit host secrets into a ≤5 MB snapshot that is persisted in `deck_snapshots` and broadcast, or into ≤1 MB op params that cross the gateway; whether CLAUDE.md's "logs/traces record only sanitized placeholders" invariant is honored for these sinks is unspecified.

### The phone runs arbitrary card HTML

`card.html` is agent-written HTML/JS executing in the user's WKWebView. The opaque-origin sandbox + the near-no-network CSP (its only affordance is the read-only `img-src baybo-transcript:` blob route, §Render host) bound the blast radius, and two isolation guards close the cross-boundary paths a card could otherwise reach through:

- **The native `deck` bridge rejects non-main-frame messages.** WKWebView injects `window.webkit.messageHandlers.deck` into *every* frame, including the sandboxed card subframe, so `DeckBridge.userContentController` guards `message.frameInfo.isMainFrame` — only the shell (main frame) may drive the native surface. Without it a card's own JS could call `quickSetup` (seeding a fresh chat draft that auto-sends — a **zero-click agent-prompt-injection channel**) or `cardAction`/`layout`/`delete`. Cards reach the shell only over their per-card MessagePort, never this handler.
- **The card SDK pins its MessagePort to the first `deck_init` from `window.parent`.** Opaque-origin frames all report origin `"null"`, so a sibling card could otherwise `postMessage` a forged `deck_init` bearing its own port to hijack a victim card's channel (read its op params, inject spoofed snapshot/result data). `sdkCard.js` accepts only the *first* init whose `source` is the shell (the card's direct parent) and ignores the rest; the shell sends `deck_init` exactly once per mount, so the once-guard never drops a real init.

One residual is inherent to the model and stays:

- **In-frame DOM injection.** The CSP uses `script-src 'unsafe-inline'` (load-bearing — a card is a self-contained inline `<script>`), so a card that renders attacker-influenced `ctx.fetch`/snapshot content via `innerHTML` executes injected inline handlers (`<img onerror=…>`). With the two guards above it is fully contained — no active network (a passive `<img>` blob load renders locally with no exfil path, §Blobs), no native bridge, no cross-card reach, opaque origin, so the injected code can only touch the card's own DOM — but the shell does no DOM sanitization; a card that renders untrusted upstream content owns escaping it.

### Admission gates code entry, not running integrity

The dry-run gate (§Authoring pipeline) covers tool-path install/update, but paths reach the running fleet around it:

- **A same-SDK direct file edit runs un-gated at next boot.** `boot()` re-gates only on `sdk` stamp drift; a hand-edit (or the agent's own non-deck `Write`/`Edit` tools) to `service.js`/`openapi.json` executes on the next restart with no dry-run, **no provenance event, and no git commit** — the "greppable audit spine" and version history capture only manager-mediated mutations.
- **`spec_hash` is not reconciled with disk at boot**, and `spec_for()` caches the compiled contract by it — so a cold-cache restart after a widening `openapi.json` edit compiles the broadened validator and stores it under the *stale* hash, enforcing an admission contract that never passed a gate while serving clients the pre-edit hash.
- **`restore()` skips both the gate and `set_installed`.** A card hand-edited while sitting in the recycle bin (delete keeps the files) goes live on restore with no dry-run and a stale `spec_hash` — the "byte-identical bundle" premise the un-gated restore relies on does not hold once the bin is editable.

### Replay, injection, validation cost

- **`x-baybo-retryable` is an unverifiable author assertion** that directly selects at-least-once replay on the phone's relay leg. The gateway cannot check it; a mislabel — even benign — makes a side-effecting op replay on any silent leg with no per-call gate. (Mandatory with no default, and `false` → never-replay, which blunts but does not close it.)
- **Op-param → shell injection is unbounded by design.** JSON-Schema admission validates param *shape*, not shell-escaping when the author interpolates a validated string into `/bin/sh -c`. Mostly owner-to-owner, but it widens the prompt-injection self-grant surface.
- **Loosened validate-time caps raise per-op cost** (params 16 KB → 1 MB over an author-controlled JSON-Schema on the `fancy-regex` ECMA engine) — but ReDoS/recursion stay blunted (`$ref` rejected, `default-features = false`, serde depth-128, fancy-regex backtrack limit), so this is a cost ceiling, not an unbounded-time hole.

## Deferred and open items

- **Web parity** (`app/web` `/deck` page) — deferred; requires extracting the shell from `app/ios/web`.
- **Egress budgets** — considered, declined; revisit only if a real card burns real quota.
- **Vendored SDK pinning** — documented escape hatch, not scheduled.
