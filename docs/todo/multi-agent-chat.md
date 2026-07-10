# Multi-agent chat — sessions bound to agent profiles

Design spec, 2026-07-10. Makes the runtime consume the `AgentProfile` entity
([`../modules/agent-profiles.md`](../modules/agent-profiles.md), management-only
today): a chat session is bound to one agent at creation, and the agent brings
its own soul (system prompt), skills, memory partition, LLM pin, and execution
framework — including `claude` / `codex` running as the top-level chat agent.

## Requirements

- **Selection**: the agent is chosen when the session is created and is
  **immutable** for the session's life. Web chat gets the picker first;
  channels / TUI / mobile keep creating builtin sessions.
- **Soul**: a custom agent's soul is `profile.system_prompt`; `NULL` inherits
  the workspace Soul (what the builtin `baybo` agent uses). `USER.md` stays
  shared — it describes the user, not the agent.
- **Skills**: shared base (builtins + `<workspace>/skills/`) visible to every
  agent, plus a per-agent skill folder overlay; same-name skills in the
  agent's folder win for that agent.
- **Memory**: one configured backend, partitioned per agent — every
  recall/write carries the session's agent id (mem0 `agent_id`, OpenViking
  `X-OpenViking-Agent`). Agent A never recalls agent B's memories.
- **Frameworks**: `framework = claude | codex` profiles serve top-level chat
  sessions through the external-agent leg. `gemini` remains
  subagent-backend-only (it is not an offered profile framework).

## Core rule: execution identity snapshots, content follows live

At creation the session stamps **who runs it**: `agent_id` and a snapshot of
the profile's `framework`. Neither ever changes for that session — editing a
profile's framework only affects new sessions, because a baybo transcript
cannot be served by an external CLI that has never seen it.

Everything that is **content** follows the profile live, resolved at use time:

| Facet | Resolved | Live effect of a profile edit |
|---|---|---|
| `system_prompt` | every context seed / post-compaction reseed | next seed/reseed picks it up |
| `llm` pin | resolved at actor spawn/hydration — an explicit per-session switch wins immediately; a profile edit lands on the next hydration | next hydration (cold start, or after an idle reap), unless the session explicitly switched models |
| agent skill folder | per-turn listing + `Skill` tool call (hot-reloadable) | next turn |
| name / avatar | display only, client-side | refetch |

## Architecture

`Router` builds/hydrates one `AgentActor` per session, as today. New step at
actor build: read `session.agent_id` (`NULL` → builtin `baybo`), load the
profile, and hand the actor a resolved **`AgentBinding`**:

```rust
pub struct AgentBinding {
    pub agent_id: AgentProfileId,        // "baybo" for unbound/builtin
    pub framework: AgentFramework,       // the session's snapshot, not the live row
    // + a store handle for live content lookups (prompt, llm pin)
}
```

**Phase 1 realization:** no separate `AgentBinding` type exists yet — the same
two fields live directly on `Session.state.{agent_id, agent_framework}`
(`SessionState::agent_id_or_builtin()` is the "baybo for unbound" accessor),
and the store handle is a plain `Arc<dyn AgentProfileStore>` threaded to the
handful of consumers that need a live read (`ContextManager`, the router's
`resolve_spawn_llm`). Same data, no extra indirection; a named `AgentBinding`
struct is worth introducing only when Phase 2's external-framework branch
needs one value to carry through the turn-dispatch seam.

- **`framework = Baybo`** — today's `AgentLoop`, parameterized by the binding:
  prompt resolution gains an agent-profile arm, skill listing takes an agent
  scope, `MemoryContext` carries `agent_id`, LLM resolution consults the pin.
- **`framework = Claude | Codex`** — the actor's turn handler dispatches each
  user turn to the existing `ExternalAgent::run()` leg (parsers, transcript
  persistence, streaming, job wrap all reused) with per-session `resume_key`
  continuity. No `AgentLoop` runs for these sessions.

**Deletion tolerance** (house style — soft references, read-time tolerance):
deleting a profile with bound sessions is allowed. A bound session whose
profile row is gone falls back to builtin behavior (workspace Soul, default
LLM, no agent skill folder) with a `warn!`. Its memory partition key stays the
stored `agent_id` string, so memories survive and stay partitioned.

## Data model & storage

Three new flat columns on `sessions`. `agent_id` / `agent_framework` follow the
`hidden` / `pinned` INSERT-seeding pattern: seeded by the session-creation
INSERT, omitted from `save`'s `DO UPDATE SET`, and with no setter — after
creation no code path can write them:

```sql
agent_id            TEXT,   -- NULL = builtin baybo (all existing rows)
agent_framework     TEXT,   -- NULL = baybo; AgentFramework::as_str() snapshot at creation
external_resume_key TEXT    -- external sessions only; write-once, from the CLI's init event
```

Phase 1 ships only `agent_id` and `agent_framework` — both are cheap guarded
`ALTER TABLE` migrations, landed and consumed as described in this document.
`external_resume_key` has no baybo-framework use, so it lands with the Phase 2
PR alongside the code that writes and reads it (also a cheap guarded ALTER —
there is no reason to pre-add an unused column).

- No `agent_profiles` schema change — the v1 shape was designed for this.
- The external chat working dir derives deterministically —
  `<workspace>/work/<kind>/chat-<session_id>/` — so it needs no column.
- Memory partition key = `agent_id` with `NULL` → `"baybo"`.
  `BUILTIN_AGENT_PROFILE_ID` is literally `"baybo"`, which is what mem0 /
  OpenViking writes already use, so existing memories stay exactly where
  builtin sessions look for them; custom agents partition under their ULID
  (rename-proof).
- Per-agent skills live at
  `<workspace>/agent-skills/<agent_id>/<skill-name>/SKILL.md` — a new
  top-level workspace dir, its own git repo, created by
  `WorkspaceManager::ensure_layout`, resolved via a new `WorkspacePaths`
  method. Keyed by profile **id**, not name, so renames don't orphan folders.

## Session creation, API, web

- **`POST /v1/chat/sessions`** gains optional `agent_id`. Write-time
  validation, crisp 400s: unknown id; external-framework profile whose backend
  is not enabled + probe-registered ("enable claude first"). Omitted / `null`
  → builtin.
- **DTOs / sync**: `SessionView` (REST + sync plane) gains `agent_id` +
  `agent_framework` so clients render the agent chip without a join; profile
  display data (name, avatar) comes from the existing `GET /v1/agents`,
  cached client-side. Standard openapi / ts-bindings regen chain.
- **Web new-chat flow**: the new-chat action opens an agent picker — avatar
  cards from `GET /v1/agents`, builtin `baybo` first and preselected so Enter
  keeps today's one-keystroke flow. Agents whose external backend is disabled
  render greyed with the reason. Conversation header + sidebar row show the
  agent avatar/name for non-builtin sessions. No new WS frames; stale
  name/avatar after an edit is refetch, not live push.
- **Model switch UI**: hidden for external-framework sessions; unchanged for
  baybo sessions.
- Channels / TUI / mobile: untouched (`agent_id NULL`).

## Baybo-framework consumption (Phase 1)

**Soul.** `ContextManager`'s prompt resolution gains one arm, priority:
subagent profile override > **agent profile `system_prompt`** (live store
fetch at seed and every post-compaction reseed; `NULL` or missing row falls
through) > workspace Soul. Subagent override and agent binding never coexist —
child sessions are not agent-bound.

**LLM.** Turn-time resolution: `last_llm ?? profile.llm (live) ?? default-llm`,
with the existing stale-pin tolerance (`warn!` + default). An explicit
per-session switch always wins; profile edits flow into sessions that never
switched.

**Skills.** `SkillRegistry` gains an agent scope: shared entries (builtins +
`<workspace>/skills/`) as today, plus per-agent entries from
`<workspace>/agent-skills/<agent_id>/`. The per-turn skill listing, the
`Skill` tool lookup, and slash-command synthesis all take the session's scope:
**shared ∪ agent folder, agent wins on name collision** (for that agent only).
`reload()` re-scans both. Risk assessment, trust levels, and validation apply
to agent skills identically (agent folders are `Trusted` workspace content;
the assessor cache already keys on content hash). `SkillInstall` /
`SkillUninstall` keep targeting the shared folder; agent folders are
hand-authored. The web Agents page's read-only skills readout switches from
the global list to "shared + this agent's folder".

**Memory.** `MemoryContext` gains `agent_id: String` (`"baybo"` for
builtin/unbound). mem0: replaces the hardcoded `agent_id: "baybo"` write
default and adds an `agent_id` filter to `recall`'s search. OpenViking: sends
`X-OpenViking-Agent: <agent_id>` instead of the hardcoded `"baybo"`. Memory
tools (`mem0_*`, `viking_*`) get the same scoping through `ToolContext`. All
hooks (`recall`, `on_job_complete`, `on_session_end`) already flow through
`MemoryContext` — partitioning is one field threaded through, no new hook
points.

## External-framework chat leg (Phase 2)

**Turn dispatch.** Each user turn on a claude/codex-bound session calls the
existing `ExternalAgent::run()` with the turn text. Continuity:
`sessions.external_resume_key` — first turn none → the CLI's init event emits
`ResumeKey` → persisted write-once (same rule as subagent resume); later turns
pass it as `--resume` / `resume <thread_id>`. Downstream is the proven
subagent leg: `Intermediate` events → `session_messages` (web transcript
renders thinking / tool_use / tool_result through the normal message
pipeline), `TextDelta` → the actor's streaming notifier → WS deltas, `Usage` →
`CostManager::record_external_tokens` (zero USD). Each turn is wrapped in a
**`UserChat` job** (not `Spawned`) with no step/span tree — the trace page
already falls back to transcript rendering for zero-step jobs.

**Soul & skills via native mechanisms.** The session's working dir is
materialized at every turn start (idempotent rewrite, so live profile edits
flow):

- `profile.system_prompt` → `CLAUDE.md` (claude) / `AGENTS.md` (codex) in the
  working dir — each CLI's own instruction-file mechanism.
- claude only: `.claude/skills` symlink → `<workspace>/agent-skills/<agent_id>/`,
  making the agent's skill folder natively visible. codex has no skill
  mechanism; it gets `AGENTS.md` only.

**Memory** works for external sessions because the hooks sit at the turn seam,
not inside `AgentLoop`: recall before `run()` (results framed into the prompt
text with the standard `<recalled_memory>` envelope), `on_job_complete` after
`FinalContent`, `on_session_end` at `ActorStop` (already framework-agnostic).

**Capability gaps — stated, not hidden** (UI copy + docs): baybo tools,
sandbox, approval gate, and secret injection do **not** apply; the security
posture equals `spawn_subagent(backend: claude)` — the CLIs run with
permissions bypassed, so treat an external-agent chat as a shell on the host.
That is why creation is gated on the operator's explicit
`external_agents.<kind>.enabled = true`. No mid-turn interjection (no tool
boundaries — mid-run messages queue in the mailbox and become the next turn).
No progress observer. No compression (context is the CLI's own problem; the
baybo-side transcript is display + memory input only).

## Error handling

| Failure | Behavior |
|---|---|
| Unknown `agent_id` at creation | 400 |
| External backend disabled/unprobed at creation | 400 "enable claude first" |
| Backend disabled after sessions exist | turn fails with a clear in-chat error; session survives, works again on re-enable |
| Profile deleted with bound sessions | builtin fallback (Soul, default LLM, no agent skills) + `warn!`; memory stays keyed to stored `agent_id` |
| Stale `profile.llm` pin | existing tolerance: `warn!` + default |
| Agent skills folder missing | empty overlay, no error |
| External CLI crash / parse error / lost resume state | turn fails visibly; `resume_key` untouched, nothing auto-cleared |
| `/stop` or job cancel on an external turn | cancel token kills the subprocess (existing `register_running` path) |

## Testing

- **Unit**: prompt-resolution priority (subagent > profile > Soul, with
  `NULL`/missing fall-through); LLM precedence; skill scope merge + collision
  override; creation validation; `sessions` column round-trips; memory
  backends assert the `agent_id` they send (mock HTTP).
- **Integration**: bind → seed carries the profile prompt; profile edit →
  reseed picks it up; profile delete → builtin fallback; memory partition
  e2e with a fake `Memory` recording `ctx.agent_id`; external leg driven by a
  fake `ExternalAgent` impl — turn dispatch, write-once resume key, transcript
  rows, `UserChat` job lifecycle. Real-CLI smokes self-skip when the binary is
  absent (sandbox-smoke pattern).
- **Web**: picker + header chip in mock mode; `scripts/check-ts-bindings.sh`
  and openapi regen gates.

## Phasing

One spec, two mergeable PRs:

1. **PR 1 — binding + baybo consumption**: `sessions` columns, creation API +
   validation, `AgentBinding`, prompt/LLM/skills/memory wiring, web picker +
   chip, `agent-skills/` workspace dir.
2. **PR 2 — external chat leg**: turn dispatch through
   `ExternalAgent::run()`, resume-key persistence, working-dir
   materialization (instruction file + skills symlink), `UserChat` job wrap,
   memory at the turn seam, creation gate on backend enablement, model-switch
   UI hiding.

## Collaboration

| Module | Role |
|---|---|
| `model` | `AgentFramework` snapshot string on sessions; no new spawn-protocol types |
| `store` / `storage` | `sessions.{agent_id, agent_framework, external_resume_key}` columns + targeted accessors; `AgentProfileStore` consumed by the runtime for live content lookups |
| `session` | `SessionView` carries `agent_id` / `agent_framework` |
| `agent` | `AgentBinding` resolution at actor build; `AgentLoop` parameterization; external turn dispatch in the actor; memory hooks at the turn seam |
| `context` | agent-profile arm in prompt resolution (seed + reseed) |
| `skills` | agent-scoped registry view; `agent-skills/` load + reload |
| `memory` | `MemoryContext.agent_id`; mem0 / OpenViking partition wiring |
| `workspace` | `agent-skills/` layout + `WorkspacePaths` accessor; external working-dir derivation |
| `gateway` | creation validation, DTOs, openapi |
| `web` | agent picker, session agent chip, Agents-page skills readout scope |
