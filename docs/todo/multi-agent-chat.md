# Multi-agent chat — one chat surface, many agents, each with its own soul, skills, and memory

Design spec, 2026-07-30. **Phase 1 is built** (see [Phasing](#phasing) for
what that covers and what it does not); Phase 2 is unstarted. Makes the
runtime consume the `AgentProfile` entity
([`../modules/agent-profiles.md`](../modules/agent-profiles.md), management-only
today): a chat session is bound to one **agent** at creation, and that agent
brings its own soul, its own skill overlay, its own memory partition, its own
LLM pin, and its own execution framework — including `claude` / `codex` running
as the top-level chat agent.

## What an agent is

An **agent** is two halves that share one identity:

- an `agent_profiles` **row** — the management surface (name, description,
  avatar, framework, llm pin), DB-authoritative, edited from the web
  **Agents** page;
- a **persona directory** — the declarative surface
  (`<workspace>/personas/<agent_id>/`), filesystem-authoritative, git-versioned,
  hand- or self-edited: `SOUL.md` and `skills/`.

The split is by *kind of content*, not by convenience. Avatars are binary and
edits are concurrent, so the row belongs in sqlite next to `sessions` and
`cron_jobs`. A soul is multi-KB markdown that **the agent itself rewrites** —
the system prompt names each identity file's absolute path and the framing tells
the model to `Edit` it (`TOP_HINT` in `crates/context/src/prompts/soul.rs`) — so
it belongs in a git repo next to `profile/`, `skills/`, and `agents/`. A soul in
a DB column would make that instruction a lie for every custom agent.

**What an agent is NOT.** Not a `SubagentProfile` (`<workspace>/agents/<name>.md`
types *spawned workers*; see [`subagent.md`](../modules/subagent.md)) — the two
registries do not read each other. Not a process: every agent runs in the same
runtime, on the same actor machinery. Not a tool boundary: `ToolRegistry` is
process-wide by design and stays that way. Not a tenant: `user_id` is unchanged,
so memory partitions by `(user, agent)` and cost still bills one owner.

## Requirements

- **Selection**: the agent is chosen when the session is created and is
  **immutable** for that session's life. Web chat gets the picker; channels and
  TUI keep creating builtin sessions.
- **Soul + self-image**: each agent owns its own `SOUL.md` (personality) and
  `IDENTITY.md` (name, creature, vibe, emoji, avatar). Only `USER.md` (who the
  human is) stays workspace-shared — there is one person however many agents
  exist.
- **Skills**: a custom agent starts with **only its own overlay** — it does not
  inherit the shared set (builtins + `<workspace>/skills/`), which belongs to
  the built-in. Granting a skill to a persona is a decision, made by putting it
  in that agent's folder. The lone exception is `baybo-cli`, which is runtime
  infrastructure rather than a capability.
- **Memory**: one configured backend, partitioned per agent — every
  recall/write carries the session's agent id. Agent A never recalls agent B's
  memories.
- **Frameworks**: `framework = claude | codex` profiles serve top-level chat
  through the external-agent leg. `gemini` stays subagent-backend-only.

## The axes, and where each one resolves

| Facet | Source of truth | Resolved at | A live edit lands |
|---|---|---|---|
| soul | `personas/<id>/SOUL.md` (builtin: `profile/SOUL.md`) | context seed + every post-compaction reseed | next seed/reseed |
| skills | `personas/<id>/skills/` | per-turn listing, `Skill` tool lookup, slash expansion | next actor spawn, or `reload()` |
| memory partition | `sessions.agent_id` | every recall / write / memory-tool call | immediately |
| llm pin | `profile.llm` | actor spawn / hydration | next hydration, unless the session switched models explicitly |
| framework | `sessions.agent_framework` (snapshot) | actor build | **never** — new sessions only |
| name / avatar | `agent_profiles` row | client-side display | refetch |

### Core rule: execution identity snapshots, content follows live

At creation a session stamps **who runs it**: `agent_id` plus a snapshot of the
profile's `framework`. Neither ever changes for that session, because a baybo
transcript cannot be served by an external CLI that has never seen it — editing
a profile's framework only affects new sessions. Everything that is *content*
follows the profile live, resolved at use time, per the table above.

### Why the binding is immutable

Three things break on a mid-thread swap, and all three break silently: the
memory partition splits (turns 1–5 are recallable only by the old agent), the
durable transcript ends up carrying two personas' output with no marker, and the
framework cannot change at all. So there is **no** `set_agent_binding` setter and
no endpoint that writes the column after creation — the same structural
guarantee `hidden` / `pinned` / `last_llm` get from INSERT-seeding.

The affordance users actually want is "continue this with a different agent",
which is a **new session** carrying provenance, not a mutation. That is
[Deferred](#deferred) as a *handoff*: create a bound session, seed it with a
summary of the origin thread plus an explicit lineage pointer. A same-framework
re-bind is technically cheap (`reseed_system_row` already exists) and can be
added later — but as a recorded, transcript-marked event, never a silent setter.

### Inheritance: which sessions carry an agent

`sessions.agent_id` means *the agent this session's work belongs to*, so it is
seeded on every session whose work originates inside a bound one:

- **top-level chat** — the picked agent (or builtin when the client sent none);
- **subagent children** — copied from the parent session row, so a `mem0_add`
  from agent A's worker lands in A's partition and the worker sees A's skill
  overlay. The automatic recall/write hooks stay off for `Spawned` turns
  (`memory_recall_query` returns `None`), so inheritance here is about the
  *tool* path and the overlay, not about the child recalling on its own. The
  child's *soul* still comes from its subagent profile: `ContextManager`'s
  priority is subagent profile > agent soul > workspace soul, because a worker's
  contract is its profile, not a persona;
- **cron fires** — copied from the cron job's origin session row (one join at
  fire time, nothing stored on `cron_jobs` — same derivation rule as
  [`cron-groups.md`](../cron-groups.md)). A job scheduled inside agent A's
  conversation fires as A, and its result lands back in A's thread with A's
  soul.

Everything else (channel sessions, TUI) leaves the column `NULL`, which reads as
the builtin.

## Data model

### `sessions`

Flat columns following the `hidden` / `pinned` INSERT-seeding pattern: written by
the session-creation INSERT, omitted from `save`'s `DO UPDATE SET`, with no
setter anywhere. Phase 1 lands the first two.

```sql
agent_id            TEXT,   -- NULL = builtin baybo (every existing row)
agent_framework     TEXT,   -- NULL = baybo; AgentFramework::as_str() snapshot at creation
external_resume_key TEXT    -- Phase 2 only; write-once, from the CLI's init event
```

`external_resume_key` has no baybo-framework use, so it lands with the Phase 2
PR alongside the code that writes it. All three are cheap guarded `ALTER TABLE`s.
`SessionState` gains the two read-side fields plus
`SessionState::agent_id_or_builtin()`, the single "NULL means builtin" accessor.

The memory partition key is `agent_id` with `NULL` → `"baybo"`.
`BUILTIN_AGENT_PROFILE_ID` is literally `"baybo"`, which is exactly what both
memory backends already send (`DEFAULT_AGENT_ID` in `mem0.rs`, the hardcoded
`x-openviking-agent`), so **existing memories stay where builtin sessions look
for them** and custom agents partition under their ULID — rename-proof.

### The persona directory

```text
<workspace_root>/
  personas/                      # standalone git repo (ensure_layout: mkdir + git init)
    01J.../                      # one dir per non-builtin agent, named by profile id
      SOUL.md                    # this agent's personality and tone
      IDENTITY.md                # this agent's self-image: name, vibe, emoji, avatar
      skills/
        deploy/SKILL.md          # private overlay; wins over a same-named shared skill
```

Keyed by profile **id**, not name, so a rename never orphans a folder. The
builtin agent's persona directory *is* the workspace's own declarative content —
`profile/SOUL.md` and the shared `skills/` — which is what makes "the assistant
you already have" an honest roster entry rather than a special case.

Three new `WorkspacePaths` methods carry that rule so no call site branches:

```rust
pub fn personas_dir(&self) -> PathBuf;                                  // <root>/personas
pub fn agent_soul_file(&self, agent: &AgentProfileId) -> PathBuf;        // builtin → profile/SOUL.md
pub fn agent_skills_dir(&self, agent: &AgentProfileId) -> Option<PathBuf>; // builtin → None
```

`agent_skills_dir` returns `None` for the builtin because the builtin *is* the
base set — an overlay pointing at `skills/` would register the same dir twice.

**Path safety.** This is the first time a profile id reaches the filesystem. Ids
are server-minted ULIDs, but `AgentProfileId` is an opaque newtype whose
`#[serde(transparent)]` `Deserialize` currently turns *any* string into one, so a
crafted request body or a hand-written row could carry `../`. The id therefore
gains a grammar — `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`, the skill-name grammar —
enforced by a fallible `AgentProfileId::parse` / `TryFrom<String>`, **and** by a
validating `Deserialize` that replaces the transparent one. A guard only on the
constructor would be bypassable by every deserialization site, which is most of
them. Nothing can then escape `personas/`, and a corrupt row is a crisp error
rather than a traversal.

### Materialization

```rust
// baybo-workspace (io feature)
pub async fn ensure_persona_layout(
    paths: &WorkspacePaths,
    agent: &AgentProfileId,
    seed_soul: &str,
) -> anyhow::Result<()>;
```

Creates `personas/<id>/skills/` and writes `SOUL.md` **only if absent**
(tmp-file + rename, like `write_identity_file`). Idempotent, and it never
overwrites — a soul the agent has since rewritten is safe. Called from two
places: `POST /v1/agents` right after the row is created, and defensively on the
actor-build path (covers rows created before this ships, and a file an operator
deleted). `workspace` stays a leaf crate: the seed *text* is passed in, because
only the gateway knows the row.

The seeds are `PERSONA_SOUL_TEMPLATE` and `DEFAULT_IDENTITY_CONTENT` in
`baybo_workspace::prompt`, both written **verbatim** — no substitution from
the profile row. The row's `name` is the operator's label (it is what the
roster sorts, what the picker shows, and what the `UNIQUE COLLATE NOCASE`
index protects); what the agent calls itself is the `Name:` slot in its own
`IDENTITY.md`, which the template invites it to choose. Interpolating the row
into either file would mint a copy nothing maintains, stale on the next
rename — so the name has exactly one source in the prompt, and it is
`<identity>`.

### `agent_profiles`

No schema change. One surface change: **`system_prompt` leaves the API.** It is
not the soul — the file is — and two prompt sources would be one too many. It is
removed from `AgentProfileDto`, from both request bodies, from
`AgentProfileUpdate`, and from the web editor, which now edits the file. The
column stays (per the storage rule, `init_db` never drops) and
`AgentProfileRow.system_prompt` stays as a documented read-only field with
exactly one consumer: `ensure_persona_layout`'s seed for an agent whose
`SOUL.md` does not exist yet. So a prompt an owner typed into the v1 editor
becomes that agent's first soul, once, and nothing writes the column again.

## Soul + self-image swap; the user profile stays shared

The assembled system prompt keeps its current shape — `TOP_HINT`, `<soul>`,
`<identity>`, `<user_profile>`, `BACKGROUND_TASKS_HINT`, `TAIL_HINT` — and what
an agent changes is **which files the `<soul>` and `<identity>` sections
read**. Both answer "who is this assistant"; `<user_profile>` answers "who is
the human", so it is the one section that cannot belong to an agent. The
`path=` attribute carries each file's absolute path, so an agent's self-edit
loop rewrites its own persona and nobody else's.

```rust
// baybo-workspace::identity
pub async fn load_soul(path: &Path, seed: &str) -> anyhow::Result<String>;
pub async fn load_shared_identity(root: &Path) -> anyhow::Result<SharedIdentity>; // { identity, user }

// baybo-context::prompts::soul
pub async fn assemble(paths: &WorkspacePaths, soul_path: &Path, soul_seed: &str) -> anyhow::Result<String>;
```

`ContextManagerConfig` gains one field, shaped exactly like the subagent one it
sits beside:

```rust
pub agent: Option<(Arc<dyn AgentProfileStore>, AgentProfileId)>,
```

Resolution order in `try_resolve_system_prompt`, at the seed and at every
post-compaction reseed:

1. **subagent profile** override (child sessions) — unchanged;
2. **agent binding** — `assemble(paths, paths.agent_soul_file(&id), seed)`, with
   the seed read live from the profile row;
3. **no binding** — `assemble(paths, profile/SOUL.md, DEFAULT_SOUL_CONTENT)`,
   byte-identical to today's behaviour.

Arms 2 and 3 are the same call with a different path, which is the point: there
is no "agent prompt" code path to keep in sync with the soul path. Arms 1 and 2
never coexist — subagent children resolve as (1) even though they carry an
inherited `agent_id` for memory and skills.

A live profile edit reaches a running session at the next seed/reseed; a live
`SOUL.md` edit reaches it the same way, since the file is re-read every time.

## Skills: one registry, two scopes

`SkillRegistry` keeps its shared map and gains a per-agent one:

```rust
pub struct SkillRegistry {
    skills: DashMap<String, SkillDefinition>,                                  // builtins + workspace skills/
    agent_skills: DashMap<AgentProfileId, HashMap<String, SkillDefinition>>,   // per-agent overlay
    load_dirs: RwLock<Vec<PathBuf>>,
    agent_dirs: RwLock<Vec<(AgentProfileId, PathBuf)>>,                        // replayed by reload()
    builtins: RwLock<Vec<SkillDefinition>>,
}
```

- `get_scoped(agent: Option<&AgentProfileId>, name)` — overlay first, then
  shared.
- `summaries_for(agent: Option<&AgentProfileId>)` — shared ∪ overlay, overlay
  wins on name collision, sorted by name for stable cross-turn ordering. Feeds
  the per-turn listing, the post-compaction skill trailer
  (`invocable_skill_summaries`), and slash candidates
  (`slash_skill_summaries`) — all three already run inside `ContextManager`,
  which now holds the scope.
- `load_agent_dir(id, path)` / boot-time scan of `personas/*/skills` — dir name
  is the agent id; `reload()` replays builtins, then `load_dirs`, then
  `agent_dirs`, snapshotting keys before mutating (the DashMap
  iterate-while-mutate footgun).
- The actor-build path calls a cheap `ensure_agent_loaded(id)` so a cold start
  picks up edits to that agent's folder without a global reload. No filesystem
  watcher — per-agent skills follow the same on-demand `reload()` rule as
  workspace skills.

Governance is unchanged and needs no new code: persona folders are workspace
content, so `TrustLevel::Trusted`; the risk assessor keys on content hash, so an
agent skill is judged like any other. `SkillInstall` / `SkillUninstall` keep
targeting the shared folder — overlays are hand-authored (see
[Phase 1.5](#phasing) for the small editor that closes the no-shell gap).

The `Skill` tool resolves through `ToolContext.agent_id`, so a private skill is
invocable only from its own agent's sessions — and an off-scope name is a plain
"unknown skill", not a refusal that leaks another agent's inventory.

## Memory: one new axis

`MemoryContext` gains a required `agent_id: AgentProfileId` (constructor
parameter, not a `with_*` setter — it is a required dependency of every call).
All four hook sites already thread `MemoryContext`, so partitioning is one field,
no new hook points, and it works for external-framework sessions for free
because the hooks sit at the turn seam rather than inside `AgentLoop`.

- **mem0** — `agent_id` on writes comes from `ctx`, and `recall`'s filter set
  gains an `agent_id` condition. `DEFAULT_AGENT_ID` survives only as the
  builtin's value.
- **openviking** — `x-openviking-agent` moves out of the client-construction
  `HeaderMap` and onto each request, sourced from `ctx`.
- **Memory tools** scope through `ToolContext.agent_id`, and the mem0 tools'
  per-call **`agentId` override is removed**. It is prompt-injectable, and with
  a partition it becomes cross-partition read *and* bulk delete
  (`mem0_delete { all: true }`). The namespace always tracks the calling
  session; openviking already has no override to remove.

That invariant — a partition the model cannot address its way out of — is what
makes a persona trustworthy, and it is why [cross-agent memory](#deferred) is
deferred rather than shipped as a profile flag.

## LLM resolution

`last_llm ?? profile.llm ?? default-llm`, resolved at actor spawn/hydration (not
per turn), with the existing stale-pin tolerance (`warn!` + default). An
explicit per-session switch always wins; a profile edit reaches sessions that
never switched at their next hydration — a cold start or an idle reap.
`last_model` and `last_effort` stay session-level: the profile pins the
`baybo.json` *entry*, and the model-within-entry plus reasoning effort are the
chat header's business. A profile-level model allow-list and effort default are a
separate follow-on feature — they change what the picker offers, not how a
session binds.

## Runtime wiring (baybo framework)

`Router`/`ActorSpawner` builds one `AgentActor` per session, as today. The
spawn closure in `crates/baybo/src/runtime.rs` gains two lines of resolution from
`session.state.agent_id_or_builtin()`:

- `ContextManagerConfig.agent` — `(agent_profile_store, agent_id)`. Both the
  soul arm and the skill scope read this one field, so a session cannot end up
  with one agent's soul and another's skills;
- `AgentLoopConfig` — the id, from which every `MemoryContext` and
  `ToolContext` the loop mints is stamped.

`initial_llm` resolution reads `session.state.last_llm` at three sites
(`router/user_input.rs` twice, `router/cron.rs` once); all three now call one
`resolve_spawn_pins(session, store)` rather than carrying three copies of the
precedence rule. It resolves before the closure, because `route_or_spawn`
takes a synchronous one and the fallback reads the store — and unbound or
built-in sessions short-circuit without touching it at all. Nothing else in
the actor changes
for a baybo-framework agent: same loop, same tools, same sandbox, same approval
gate, same compression.

Prompt caching is unaffected — the system row already varies per session.

## Selection, API, and wire

- **`POST /v1/chat/sessions`** gains optional `agent_id`. Write-time validation
  with crisp 400s: unknown id; external-framework profile whose backend is not
  enabled and probe-registered ("enable claude first"). Omitted / `null` →
  builtin. Creation goes through `create_session_with_agent` /
  `get_or_create_with_agent`, which do idempotent-retry plus compare-after-create
  race detection, since the columns are INSERT-seeded and cannot be patched
  afterwards.
- **`GET /v1/agents`** (exists) is the roster; the picker and the chip read it.
- **`GET`/`PUT /v1/agents/{id}/soul`** and **`…/identity`** — the two files an
  agent owns, `{ content, path, version }` out and `{ content, version? }` in,
  a full replace written tmp-file + rename, answering with the new state so an
  open editor holds a fresh base. Both resolve through
  `AgentProfileId::identity_file`, so for the builtin they edit
  `profile/{SOUL,IDENTITY}.md`: the Agents page is the one place to edit *any*
  agent's persona, and the builtin's "locked except avatar" rule is untouched
  (these are files, not row fields).

- **The display name is not a column.** `POST` / `PUT /v1/agents` accept a
  `name` and splice it into that agent's `IDENTITY.md`; every read derives it
  back out, falling back to the id when the file names nothing. So an operator
  renaming an agent and the agent renaming itself are the *same* write to the
  *same* line, and no synchronisation exists to go wrong. The cost is honest
  and priced: `GET /v1/agents` becomes one query plus a concurrent file read
  per agent, names are no longer unique or SQL-sortable, and the roster's
  order is computed after the reads. None of that touches correctness — the
  **id** is the identity everything else keys off, so a duplicate name is a
  display ambiguity, not a collision.

  **`version` is a compare-and-set token**, not an optimisation. Clients here
  are *routinely* stale by design — the page neither polls nor subscribes, and
  the agent rewrites these files mid-conversation through `Edit`. Without the
  precondition, a Save from an editor opened before a self-edit would silently
  delete what the agent wrote; with it, the write is refused (409) and the
  operator re-reads. An absent `version` stays unconditional, for a caller
  that genuinely means "set it to this". Stale *display* is fine; stale
  *writes* are data loss.
- **`GET /v1/skills?agent_id=`** returns shared ∪ that agent's overlay, with a
  flag marking which entries are private. Feeds the Agents-page readout.
- **`GET /v1/chat/slash-manifest?agent_id=`** — the manifest is fetched once at
  chat bootstrap today; scoping it means passing the active conversation's agent
  and refetching on conversation switch (cache per agent client-side). Worth
  doing in Phase 1: an agent whose private `/deploy` works when typed but is
  invisible in completion undercuts the whole feature.
- **DTOs / sync**: `SessionView` (REST + sync plane) gains `agent_id` and
  `agent_framework` so a client renders the chip without a join; name and avatar
  come from the cached roster. Standard openapi + `check-ts-bindings.sh` regen.
- Channels and TUI are untouched (`agent_id NULL`). Their process-global slash
  menu stays exactly right, because a session they create is always builtin.

## Web UI

- **New chat** opens an agent picker: avatar cards from `GET /v1/agents`,
  builtin first and preselected so Enter keeps today's one-keystroke flow.
  Agents whose external backend is disabled render greyed with the reason.
- **Conversation header + sidebar row** show the agent's avatar and name for
  non-builtin sessions.
- **Agents page** replaces the `system_prompt` textarea with a soul editor bound
  to `…/soul` (showing the on-disk path, so the operator knows what to commit),
  and the read-only skills readout switches to "shared + this agent's private
  overlay".
- **Model switch** is hidden for external-framework sessions; unchanged
  otherwise.
- No new WS frames: a stale name/avatar after a profile edit is a refetch, not a
  live push.

## External frameworks as top-level chat (Phase 2)

**Turn dispatch.** Each user turn on a claude/codex-bound session calls the
existing `ExternalAgent::run()` with the turn text; no `AgentLoop` runs.
Continuity is `sessions.external_resume_key`: first turn none → the CLI's init
event emits `ResumeKey` → persisted write-once (the subagent rule), later turns
pass it back. Downstream is the proven subagent leg — `Intermediate` events →
`session_messages` (thinking / tool_use / tool_result render through the normal
pipeline), `TextDelta` → the streaming notifier → WS deltas, `Usage` →
`CostManager::record_external_tokens` (zero USD). Each turn is wrapped in a
`UserChat` turn row with no step/span tree; the trace page already falls back to
transcript rendering for zero-step turns.

**Soul and skills via the CLIs' own mechanisms.** The session's working dir
(`<workspace>/work/<kind>/chat-<session_id>/`, derived, so no column) is
materialized at every turn start — an idempotent rewrite, so live edits flow:

- `personas/<id>/SOUL.md` → `CLAUDE.md` (claude) / `AGENTS.md` (codex);
- claude only: `.claude/skills` symlink → `personas/<id>/skills/`. Codex has no
  skill mechanism and gets the instruction file alone.

The persona directory maps onto each CLI's native conventions with no
translation layer, which is the second reason the soul is a file.

**Memory** works unchanged: recall before `run()` (framed with the standard
`<recalled_memory>` envelope), `on_turn_complete` after `FinalContent`,
`on_session_end` at `ActorStop`.

**Capability gaps — stated, not hidden** (UI copy and docs): baybo tools,
sandbox, approval gate, and secret injection do **not** apply. The security
posture equals `spawn_subagent(backend: claude)` — the CLIs run with permissions
bypassed, so an external-agent chat is a shell on the host. That is why creation
is gated on the operator's explicit `external_agents.<kind>.enabled = true`.
No mid-turn interjection (no tool boundaries — mid-run messages queue in the
mailbox and become the next turn), no progress observer, no compression (context
is the CLI's problem; the baybo-side transcript is display plus memory input).

## Error handling

| Failure | Behaviour |
|---|---|
| Unknown `agent_id` at creation | 400 |
| `agent_id` failing the id grammar (corrupt row / crafted path) | hard error at parse; never touches the filesystem |
| External backend disabled or unprobed at creation | 400 "enable claude first" |
| Backend disabled after sessions exist | turn fails with a clear in-chat error; the session survives and works again on re-enable |
| Profile row deleted with bound sessions | builtin fallback (workspace soul, default LLM, no overlay) + `warn!`; memory stays keyed to the stored `agent_id`, so it survives and stays partitioned |
| `personas/<id>/SOUL.md` missing | re-seeded from the template (or the legacy row prompt) and used |
| `personas/<id>/SOUL.md` unreadable (I/O error) | fall back to the workspace soul + `warn!`; never fail the turn |
| Persona directory deleted while sessions live | empty overlay, soul re-seeded; no error |
| Stale `profile.llm` pin | existing tolerance: `warn!` + default |
| External CLI crash / parse error / lost resume state | turn fails visibly; `resume_key` untouched, nothing auto-cleared |
| `/stop` on an external turn | the cancel token kills the subprocess (existing `register_running` path) |

Persona directories are never swept. A deleted profile leaves its folder inert,
exactly like an orphaned avatar blob — per CLAUDE.md there is no background
cleanup of any kind.

## Testing

- **Unit** — prompt resolution across all three arms (subagent > agent soul >
  workspace soul) including the missing-file and unreadable-file fallbacks;
  `agent_soul_file` / `agent_skills_dir` builtin-vs-custom mapping; the id
  grammar rejecting traversal; `ensure_persona_layout` idempotence and
  never-overwrite; skill scope merge plus collision override; LLM precedence;
  creation validation; `sessions` column round-trips and the absence of any
  post-INSERT writer; both memory backends asserting the `agent_id` they send
  (mock HTTP), plus a regression test that the mem0 tools no longer accept
  `agentId`.
- **Integration** — bind → seed carries the agent's soul; edit `SOUL.md` →
  reseed after compaction picks it up; profile delete → builtin fallback; memory
  partition e2e with a fake `Memory` recording `ctx.agent_id()`; a subagent
  spawned from a bound session inheriting the partition; a cron fire inheriting
  it from its origin session; the external leg driven by a fake `ExternalAgent`
  (turn dispatch, write-once resume key, transcript rows, turn lifecycle).
  Real-CLI smokes self-skip when the binary is absent (sandbox-smoke pattern).
- **Web** — picker, header chip, and soul editor in mock mode; the scoped
  slash-manifest refetch on conversation switch; openapi and
  `scripts/check-ts-bindings.sh` gates.

## Phasing

1. **Phase 1 — binding + baybo consumption.** *Built*: `sessions` columns and
   the write-once binding, `POST /v1/chat/sessions { agent_id }` with
   validation and persona materialization, the `personas/` layout, soul
   assembly by path (with the deleted-profile and unreadable-file
   fallbacks), the skill overlay and its scoped lookups, the memory partition
   across both backends plus the removal of the `agentId` override, the
   `…/soul` endpoints, and the Agents-page soul editor.
   *Also built since*: the per-agent `IDENTITY.md`, the name living in it
   rather than a column, conditional writes on the identity files, skills no
   longer inherited by a custom agent, the agent-scoped `GET /v1/skills`, LLM
   precedence through the shared `resolve_spawn_pins`, and agent inheritance
   for subagent children and cron fires.
   *Still open in Phase 1*: the web new-chat picker and session chip,
   `SessionView` carrying the binding, and the agent-scoped slash-manifest.
2. **Phase 1.5 (optional, small).** Authoring an overlay skill from the web:
   `PUT /v1/agents/{id}/skills/{name}` writing one `SKILL.md` through the same
   scoped-file seam as the soul endpoint, then `reload()`. Closes the "owner has
   no shell" gap without taking on multi-file skill upload.
3. **Phase 2 — external chat leg.** Turn dispatch through
   `ExternalAgent::run()`, `external_resume_key`, working-dir materialization,
   turn wrap, memory at the turn seam, creation gate on backend enablement,
   model-switch UI hiding.

## Deferred

- **Handoff** — "continue this thread with another agent" as a new bound session
  seeded with a summary plus a lineage pointer, and (if wanted) a recorded
  same-framework re-bind.
- **Cross-agent memory** — a shared pool would need a per-write policy and a
  recall union, and it reopens exactly the cross-partition access the removed
  `agentId` override closed. Not until there is a concrete use case.
- **Per-agent tools** — `ToolRegistry` is process-wide by design; a per-agent
  allow-list means the executor consults the binding on every call.
- **Per-agent work dir** — baybo agents share `work/`; only external sessions get
  their own tree today.
- **Per-agent cost and trace analytics** — the join already exists
  (`cost_records` → `turns` → `sessions.agent_id`); add a column only if the
  surface becomes first-class.
- **@-mention / slash agent selection** — unique profile names are already
  reserved for it.
- **Markdown export/import of a whole agent** — the persona directory is already
  git-versioned; the row is the only part not in a repo.

## Collaboration

| Module | Role |
|---|---|
| `model` | `AgentProfileId` grammar validation, `AgentFramework` snapshot on sessions, `SessionState::{agent_id, agent_framework, agent_id_or_builtin}` |
| `workspace` | `personas/` in `ensure_layout` (+ git init), `personas_dir` / `agent_soul_file` / `agent_skills_dir`, `ensure_persona_layout`, `load_soul` / `load_shared_identity`, `PERSONA_SOUL_TEMPLATE` |
| `store` / `storage` | `sessions.{agent_id, agent_framework, external_resume_key}`; `AgentProfileRow.system_prompt` demoted to a seed-only read |
| `context` | `ContextManagerConfig.agent`, the agent arm in prompt resolution (seed + reseed), scoped skill listings / trailer / slash candidates |
| `skills` | `agent_skills` overlay map, `get_scoped` / `summaries_for` / `load_agent_dir` / `ensure_agent_loaded`, `reload()` replay |
| `memory` | `MemoryContext.agent_id`; mem0 filter + write scoping, openviking per-request header, `agentId` override removed |
| `tools` | `ToolContext.agent_id`; scoped `Skill` tool lookup |
| `agent` | binding resolution at actor build, `resolve_spawn_llm` consulting the profile, external turn dispatch (Phase 2) |
| `session` | `SessionView.{agent_id, agent_framework}`; inheritance at child/fire creation |
| `gateway` | creation validation, `…/soul` endpoints, scoped `/v1/skills` and `/v1/chat/slash-manifest`, openapi |
| `web` | agent picker, session chip, soul editor, scoped skills readout and slash completion |
