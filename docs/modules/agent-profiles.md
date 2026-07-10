# agent-profiles — user-managed chat personas (`AgentProfile`)

## Overview

Agent profiles are user-managed personas: a named, avatar-carrying bundle of system prompt, an execution framework (`baybo` / `claude` / `codex`), and an optional LLM pin. The operator creates and edits them from the web dashboard's **Agents** page. A web chat session can bind to one at creation time — see [Session binding](#session-binding) — and from then on the profile drives that session's system prompt, LLM pin, and skill overlay. Skills are **not** a profile field — the editor displays them read-only, live from the skill registry (managed by the skill system, not configured per agent here).

The feature ships the entity and its persistence, full CRUD over `/v1/agents`, the web **Agents** page, and the `baybo`-framework runtime consumer: session creation, prompt resolution, LLM precedence, skill scoping, and memory partitioning all read `agent_profiles` for a session bound to a `baybo`-framework agent. `claude` / `codex` profiles are storable and editable today but stay management-only — the external-agent chat leg that would actually run a top-level session under those frameworks is [Deferred](#deferred).

This is a cross-crate feature subsystem, not a crate. The pieces live where their kind of code already lives:

- `crates/model/src/agent_profile.rs` — `AgentProfileId`, `AgentFramework`, `BUILTIN_AGENT_PROFILE_ID`, `MAX_AGENT_PROFILE_NAME_CHARS`.
- `crates/store/src/agent_profile.rs` — `AgentProfileRow`, `AgentProfileUpdate`, the `AgentProfileStore` trait.
- `crates/storage/src/libsql/agent_profile.rs` — `LibsqlAgentProfileStore` + the `agent_profiles` table in `init_db()`.
- `crates/gateway/src/api/admin/agents.rs` — the `/v1/agents` handlers and DTOs.
- `app/web/src/pages/AgentsPage.tsx` — the management page.

**What this is NOT.** It is not `SubagentProfile` ([`subagent.md`](subagent.md)): that is the filesystem-authoritative registry (`<workspace>/agents/<name>.md`, `DashMap`, disk wins on `reload`) that types *spawned subagents*. Agent profiles are DB-authoritative, web-managed, and aimed at *top-level* sessions. The two registries do not read each other, share no types beyond `baybo-model`, and a name appearing in both means nothing. It is also not the Soul: the workspace `profile/` identity files remain the default persona, and an agent profile only ever *overrides* them (a `NULL` prompt means "use the Soul").

## Data model

```rust
// crates/store/src/agent_profile.rs
pub struct AgentProfileRow {
    pub id: AgentProfileId,            // opaque string, ULID at genesis (mirrors FolderId)
    pub name: String,                  // display name; unique, ASCII case-insensitive
    pub description: String,
    pub avatar_blob_id: Option<String>,// full blob id incl. read token, from POST /v1/blobs
    pub system_prompt: Option<String>, // None = workspace Soul (default behavior)
    pub framework: AgentFramework,     // baybo | claude | codex
    pub llm: Option<LlmEntryName>,     // None = follow default-llm; meaningful for baybo only
    pub builtin: bool,                 // the seeded `baybo` row; locked except avatar
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

`AgentFramework` is a flat unit enum in `baybo-model` (`#[serde(rename_all = "snake_case")]` + the house `as_str()` / `parse()` / `const ALL` mirror): `Baybo | Claude | Codex`, `Baybo` the `Default`. Its string forms are a subset of the spawn protocol's backend tags — `BAYBO_BACKEND_TAG` and `ExternalAgentKind::as_str()` (the runtime also has `Gemini`, deliberately not offered as an agent-profile framework) — and it carries `to_backend_kind(self) -> SubagentBackendKind` so future runtime wiring is a lossless mapping, not a string translation.

`system_prompt` is markdown text, matching the Soul files it overrides. `NULL` consistently means **inherit the default**:

| Field | `NULL` / `None` | `Some(...)` |
|---|---|---|
| `system_prompt` | workspace Soul (`assemble_from_workspace`), same as every session today | replaces the Soul for bound sessions |
| `llm` | follow `default-llm` | pin to this `baybo.json` entry name |

`llm` is stored regardless of `framework` (the server never clears it on a framework switch, so switching never destroys data), but it is genuinely baybo-only: it names a `baybo.json` LLM-pool entry, which an external CLI (billed against its own subscription) can't route through — the editor greys it out for external frameworks. `system_prompt` and `llm` are both consumed live by a bound `baybo`-framework session — see [Session binding](#session-binding).

**Neither skills nor tools are stored on the profile.** Skills are managed by the skill system, not configured per agent — the editor reads them live from the skill registry (`GET /v1/skills`, `?agent_id=` scoped to the profile's own overlay folder once one is selected — see [Session binding](#session-binding)) and shows them read-only. Tools are a runtime-global concern (`ToolRegistry` is process-wide by design) and Claude Code / Codex manage their own tool permissions. Storing either as a per-agent allow-list would be dead data, so both are left out.

### The built-in `baybo` profile

Exactly one row is seeded with `id = BUILTIN_AGENT_PROFILE_ID` (`"baybo"`), `name = "baybo"`, `builtin = 1`, `framework = baybo`, every nullable field `NULL`, and its description from the `BUILTIN_AGENT_PROFILE_DESCRIPTION` const in the libsql impl — the row *is* the default behavior. It is **read-only except its avatar** and cannot be deleted: the agent list always has an honest entry for "the assistant you already have", and the session-binding picker (below) gets a default target without special-casing "no profile".

## Session binding

A web chat session is bound to at most one agent profile, **at creation only** — binding is immutable for the session's life. This is the runtime consumer the schema was shaped for.

### Data model: two flat columns, write-once

`sessions` carries `agent_id TEXT` and `agent_framework TEXT` (the `last_llm` anti-clobber pattern — no state-blob write): `NULL` on both means the builtin `baybo` agent, which is every pre-binding row. `SessionState::agent_id_or_builtin()` is the one accessor every consumer uses to turn "maybe bound" into a concrete id — it returns the bound `agent_id` or `BUILTIN_AGENT_PROFILE_ID`.

`SessionStore::set_agent_binding(session_id, agent_id, framework)` is a **targeted, write-once** setter: it only fires `UPDATE … WHERE agent_id IS NULL`, so a session can be bound exactly once and a second call is a no-op (`Ok(false)`). There is no unbind and no rebind — an agent's `framework` is a snapshot of the profile at bind time, not a live read, because a baybo transcript can't be served by an external CLI that has never seen it (framework itself isn't consumed yet; only `baybo`-framework binding is wired through the runtime — see below).

### Creation API

`POST /v1/chat/sessions` takes an optional `agent_id` in the body. Resolution (`resolve_agent_binding` in `crates/gateway/src/api/admin/chat.rs`) is write-time validation with crisp 400s:

| `agent_id` | Result |
|---|---|
| absent / `null` / `"baybo"` (`BUILTIN_AGENT_PROFILE_ID`) | unbound — `agent_id`/`agent_framework` stay `NULL` |
| unknown id | 400 |
| a known id whose profile `framework != baybo` | 400 "external-framework chat sessions are not supported yet" |
| a known `baybo`-framework id | bound via `set_agent_binding` |
| any `agent_id` on a request that resolves to an **existing** session (a client-supplied `session_id` that already has a row) | 400 "cannot set an agent on an existing session" — binding only happens at true creation |

`ChatSessionSummary` and `ChatSessionDetail` (the REST + sync DTOs) carry `agent_id` / `agent_framework` so the client renders the agent chip without a join back to `/v1/agents`; `SessionPatch.agent_id` rides the session-created broadcast and the unhide broadcast (reconstructing a hidden row's chip without a refetch — an unhide is the other place a client first learns about a row it may not have cached).

### Content resolution: identity snapshots, content follows live

The framework snapshot never changes; everything the profile actually contributes is resolved live, at use time, by session's `agent_id_or_builtin()` / `agent_id`:

- **System prompt.** `ContextManager::try_resolve_system_prompt` gains an agent-profile arm below the subagent-profile arm and above the workspace Soul: for a bound session it does a live `AgentProfileStore::get` and uses `row.system_prompt` if `Some`. A `NULL` prompt, a missing row (deleted profile), or a store error all fall through to the workspace Soul with a `warn!` — never a hard failure. This runs both at context seed and at every post-compaction reseed, so an edited prompt lands on the next reseed without restarting the session.
- **LLM pin.** `resolve_initial_llm` (`crates/agent/src/actor/router/user_input.rs`) is the precedence: the session's explicit `last_llm` pin wins if set; otherwise the bound profile's `llm` is read live from the store; otherwise the pool default. This resolves **at actor spawn / hydration**, not per turn — a profile edit lands the next time the actor is (re)built (cold start, or after an idle reap), while an explicit per-session model switch (`PUT /v1/chat/sessions/{id}/model`) always wins immediately via the existing `AgentMessage::SetModel` re-pin.
- **Skills.** The agent's overlay folder at `<workspace>/agent-skills/<agent_id>/` is layered on the shared skill set for every listing, lookup, and slash expansion the session makes — see [`skills.md`](skills.md#agent-scoped-overlay).
- **Memory.** The partition key is `agent_id_or_builtin()` — `"baybo"` for unbound sessions (the backends' default agent namespace, so unbound sessions and pre-binding memories share one partition), the profile's ULID for a bound one. See [`memory.md`](memory.md#partitioning-by-agent).

### Deletion tolerance

Deleting a profile with bound sessions is allowed (house style — soft references, read-time tolerance, no FK). A bound session whose profile row is gone falls back to builtin behavior — workspace Soul, default LLM, no agent skill overlay — with a `warn!` at each resolution site, never a session-breaking error. Its memory partition key stays the stored `agent_id` string (not `BUILTIN_AGENT_PROFILE_ID`), so its memories keep their own partition instead of silently merging into the builtin one.

### Not yet wired: subagents and external frameworks

Subagent sessions (`SubagentRegistry` / the spawn router) are never agent-bound — `ContextManager`'s subagent-profile arm and its agent-profile arm are mutually exclusive by construction (a session has `subagent_profile` xor `agent_profile`, never both). A `claude`/`codex` profile can be created and edited, but creating a top-level chat session against one 400s (see the creation-API table above) until the external-agent chat leg lands — [Deferred](#deferred).

## Design Decisions

### DB-backed, not workspace files

Web CRUD is the primary interface, avatars are binary, and edits are concurrent — that is the profile of the libsql-managed entities (`sessions`, `session_folders`, `cron_jobs`), not of the git-versioned workspace markdown (Soul `profile/`, `skills/`, subagent `agents/`). The DB row is the single source of truth; there is no file mirror, no watcher, no `reload()` semantics.

### The builtin row is locked structurally, not by convention

`AgentProfileStore::update` and `::delete` execute with `WHERE id = ?1 AND builtin = 0`, so a builtin mutation affects zero rows no matter who calls the store — the gateway's crisp 400 is UX on top, not the enforcement. `set_avatar` is the one write without the guard, because the avatar is the one field the builtin allows. The `builtin` column is written only by the seed: `create`'s `INSERT` omits the column (schema `DEFAULT 0` fills it), so a second `builtin = 1` row — which the guards would make permanently un-editable and un-deletable — is unmintable through the store.

### Seeding is `INSERT OR IGNORE` at store open

`LibsqlAgentProfileStore::open(pool)` (async, mirroring `LibsqlBlobStore::open`) runs the builtin seed after `init_db()`: `INSERT OR IGNORE` with the fixed id, so a fresh DB gets the row and an existing DB keeps it — including a user-set avatar, because ignore-on-conflict never touches the live row. `init_db()` itself stays pure DDL, and there is no boot-time re-assert of the other builtin fields: no write path can reach them.

### The `llm` pin is validated at write, tolerated at read

`llm` is validated against the live pool on write (via the shared `validate_llm_pin`, exactly like the session model switch) — a crisp 400 at edit time beats a silent fallback. Staleness later (the entry removed from `baybo.json` after the profile saved it) is tolerated the same way `sessions.last_llm` staleness is: the pool's `resolve` falls back to default with a `warn!` when consumption arrives; the editor renders a stale pin as "(unavailable)" so it stays visible and clearable.

### Full-replace `PUT`, targeted avatar endpoint

Nullable fields where `NULL` is meaningful make a partial `PATCH` need absent-vs-null tri-state, which serde/utoipa express badly. So content updates are a **full replace**: the body carries the complete content state — `name`/`description`/`framework` **required** (so an omitted `framework` can't silently reset a profile to `baybo`), absent nullable fields reset to `NULL`. Racing `PUT`s are last-write-wins, no version precondition. The avatar is excluded and lives on its own targeted endpoint (house style: `…/model`, `…/pin`, `…/folder`) — that is what maps "builtin is locked except avatar" onto endpoints instead of per-field conditional validation.

### Uniqueness and identity

`id` is an opaque string minted server-side (`AgentProfileId::generate()`, a ULID — the `FolderId` pattern); `name` is the mutable display name, free-form after trim, non-empty, at most `MAX_AGENT_PROFILE_NAME_CHARS` (64) characters, and unique via `UNIQUE` + `COLLATE NOCASE` (ASCII case-insensitive — good enough to keep "Baybo" from shadowing the builtin `baybo`). A violation surfaces as `StorageError::Conflict` → 400. Unique names keep the list legible next to the builtin and leave room for later @-mention-style selection.

### Avatars ride the existing blob pipeline

No new image storage: the client uploads via the existing `POST /v1/blobs` and stores the returned `blob_id` — the full `sha256:<digest>.<read-token>` capability string — on the profile. Every path that persists an `avatar_blob_id` (`PUT …/avatar` **and** a `POST` create that supplies one) `stat`s the blob first and 400s on an unknown id or a stored mime outside `image/*`; error messages redact the read token. After that the reference is soft (FKs are off workspace-wide). No avatar-specific size cap — the blob pipeline's `MAX_BLOB_BYTES` is the only bound, deliberately. Replaced avatars orphan their old blob **inert** — per the repo rule, no cleanup sweeper.

## Storage

```sql
CREATE TABLE IF NOT EXISTS agent_profiles (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE COLLATE NOCASE,
    description     TEXT NOT NULL,
    avatar_blob_id  TEXT,
    system_prompt   TEXT,
    framework       TEXT NOT NULL,          -- AgentFramework::as_str()
    llm             TEXT,
    builtin         INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,       -- Unix µs (libsql/time.rs)
    updated_at      INTEGER NOT NULL
);
```

House encodings throughout: timestamps µs via `super::time`, booleans `INTEGER` 0/1 (an unknown `framework` string on read is an error, never a silent fallback). No `skills` column — skills are read live from the registry, not stored per agent.

```rust
// crates/store/src/agent_profile.rs
#[async_trait]
pub trait AgentProfileStore: Send + Sync {
    async fn list(&self) -> Result<Vec<AgentProfileRow>>;            // ORDER BY builtin DESC, name COLLATE NOCASE
    async fn get(&self, id: &AgentProfileId) -> Result<Option<AgentProfileRow>>;
    async fn create(&self, row: &AgentProfileRow) -> Result<()>;     // duplicate name → Conflict; never binds `builtin`
    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool>; // WHERE builtin = 0; duplicate name → Conflict
    async fn set_avatar(&self, id: &AgentProfileId, blob_id: Option<&str>) -> Result<bool>;   // builtin allowed
    async fn delete(&self, id: &AgentProfileId) -> Result<bool>;     // WHERE builtin = 0; plain row DELETE
}
```

`AgentProfileUpdate` is the full content state minus `id`/`avatar_blob_id`/`builtin`/timestamps; `update` and `set_avatar` bump `updated_at`. Both name-writing methods map the `UNIQUE` violation to `StorageError::Conflict` (the device-store message-sniff); a case-only self-rename is not a conflict. `Ok(false)` = no row matched (missing id, or the builtin behind the guard) — the gateway `get`s first to disambiguate, and reads `Ok(false)` after a non-builtin `get` as a concurrent delete → 404. The store stays a dumb writer — name/llm/blob validation lives in the gateway handlers — and rides the `Store.agent_profile` bundle field out of `Store::open`.

## HTTP API

Route module `crates/gateway/src/api/admin/agents.rs`, tag `agents`, DTOs inline per the folders precedent. `AgentProfileDto` mirrors the row — `framework` as an enum mirror, timestamps RFC3339, nullable fields skipped when `None`. No `skills` field on any agent DTO (create, update, or response); the editor sources skills from the separate `GET /v1/skills`.

| Endpoint | Success | Errors |
|---|---|---|
| `GET /v1/agents` | 200 `ListResponse<AgentProfileDto>` | 401 |
| `POST /v1/agents` | 200 `AgentProfileDto` | 400 name invalid/duplicate, unknown `llm`, unknown/non-image `avatar_blob_id`; 401 |
| `GET /v1/agents/{agent_id}` | 200 `AgentProfileDto` | 401, 404 |
| `PUT /v1/agents/{agent_id}` | 204 | 400 builtin locked, name invalid/duplicate, unknown `llm`; 401, 404 |
| `PUT /v1/agents/{agent_id}/avatar` | 204 | 400 unknown blob id / non-image mime; 401, 404 |
| `DELETE /v1/agents/{agent_id}` | 204 | 400 builtin; 401, 404 |

- `POST` body = full content state (`name` required; `description` defaults empty; `framework` defaults `baybo`; nullable fields absent = `NULL`) **plus** optional `avatar_blob_id` — create can't hit the builtin lock, so no avatar asymmetry is needed there. `PUT` body = the same shape minus `avatar_blob_id`, with `name`/`description`/`framework` required.
- `PUT …/avatar` body = `{ "blob_id": "sha256:…" | null }`; `null` clears the avatar.
- Builtin refusals are `GatewayError::BadRequest` (the admin surface's "operation not allowed" convention — there is no `Forbidden` variant), with the store's `builtin = 0` guard as backstop.
- `AdminState` carries `agent_profile_store` and `blob_store` (for the avatar `stat`), both off `deps.stores`.

Shape changes ride the standard openapi regen chain — see the header of `crates/gateway/src/api/dto.rs`.

## Web UI

`AgentsPage.tsx` + an `/agents` route + an `IconRail` destination, following the house conventions (hand-rolled fetch with refetch-after-mutation, `useAdminClient()`, 401 → `logout()`, `?mock=true` with every mutation short-circuited in mock mode). Layout is a `[sidebar | detail]` split like the chat page.

- **Sidebar**: the agent roster — per-row character face (uploaded avatar > bundled brand image for the builtin > monogram on a deterministic per-agent tint), name (+ lock icon on the builtin), `framework · model` subtitle, coral highlight on the active row; a "New agent" button switches the detail pane to the create form.
- **Detail**: an inline character-sheet editor (no modal, centered single column), keyed by selection. Header: large avatar portrait + image/remove controls beside the name field, a fixed-min-height meta row (builtin lock badge / id) so switching agents never jumps the layout. Body: description, framework, LLM pin, system prompt, then a full-width **read-only skills readout** — every skill visible to that agent listed with its `SKILL.md` description (`GET /v1/skills` unscoped for the builtin/no-selection/create-new state, `?agent_id=<id>` scoped to that profile's `agent-skills/` overlay otherwise — mirrors `SkillRegistry::summaries_for_agent`, see [`skills.md`](skills.md#agent-scoped-overlay)). A removed LLM entry renders as "(unavailable)"; the LLM pin greys out for external frameworks ("baybo only"). Footer: destructive Delete on the left (custom agents; a confirm dialog follows), Save on the right. For the builtin everything except the avatar is disabled and Save is live only when the avatar changed.
- **Avatar**: file input → `POST /v1/blobs` → `PUT /v1/agents/{agent_id}/avatar`; rendering fetches the blob with the bearer into an object URL (an `<img>` can't carry the auth header). The bundled builtin default is `app/web/src/assets/baybo-avatar.webp` (a 256² webp squeezed from `assets/baybo.png`; `avatar_blob_id` stays `NULL`).
- **No WS for profile edits**: nothing pushes agent-profile CRUD live — no `Frame` variant, no store/context; the Agents page is refetch-driven. A session's *binding*, by contrast, does ride the wire: `SessionPatch.agent_id` rides the chat page's session-created and unhide broadcasts (see [Session binding](#session-binding)), so sibling tabs render the agent chip without a refetch.
- **Chat picker + chips** (`app/web/src/pages/chat/AgentPicker.tsx`): a popover on the sidebar's New-chat button, sourced from `GET /v1/agents` — builtin first and preselected so Enter keeps the one-keystroke flow, other agents sorted by name, external-framework agents rendered disabled with a "not supported yet" title. A bound session shows a monogram chip in the sidebar row and the conversation header (name/avatar data joined client-side from the same `GET /v1/agents` fetch, not carried per-message).

## Constraints

- Feature subsystem, not a crate: no `baybo-agent-profiles` crate until there is behavior beyond CRUD. Domain types in `model`, port in `store`, impl in `storage`, policy in `gateway` handlers.
- **Runtime coupling is confined to `baybo`-framework binding.** A bound session resolves prompt/LLM/skills/memory live from the profile (see [Session binding](#session-binding)); the spawn router and `SubagentRegistry` are untouched — subagent sessions are never agent-bound, and `claude`/`codex` profiles cannot be bound to a top-level session yet. Deleting a bound profile is safe by construction: every live-read site tolerates a missing row and falls back to builtin behavior with a `warn!` (see "Deletion tolerance" above) rather than erroring the session.
- Strictly disjoint from `SubagentProfile` and from the workspace Soul; the only shared vocabulary is `baybo-model` (`ExternalAgentKind`, `LlmEntryName`, the backend tag strings).
- All cross-entity references are soft (FKs are off): `avatar_blob_id` into `blobs`, `llm` into `baybo.json`, `sessions.agent_id` into `agent_profiles`. Write-time validation where it's cheap and crisp (llm, avatar, session-creation `agent_id`), tolerance at read time everywhere.
- Only `name` carries an explicit length bound (`MAX_AGENT_PROFILE_NAME_CHARS`); `description` and `system_prompt` are deliberately bounded only by the admin request-body limit.
- Profile rows are user data with a normal delete affordance — the session never-delete rule does not apply — but there is still no background sweeper of any kind, and orphaned avatar blobs stay inert.

## Deferred

- **External-framework top-level sessions** — `claude`/`codex`/`gemini` currently run only as subagent backends; a profile with an external framework needs the external-agent leg generalized to top-level chat, including `sessions.external_resume_key` and working-dir materialization. (`gemini` exists as a runtime backend but isn't offered as an agent-profile framework.) See [`../todo/multi-agent-chat.md`](../todo/multi-agent-chat.md) "External-framework chat leg (Phase 2)".
- **Markdown export/import** — if git-versioning of personas ever matters; DB stays authoritative.
- **@-mention / slash selection** — unique names are already reserved for it.

## Collaboration

| Module | Role |
|---|---|
| `model` | `AgentProfileId` (ULID-minted string newtype), `AgentFramework` (+ `to_backend_kind`), `BUILTIN_AGENT_PROFILE_ID`, `MAX_AGENT_PROFILE_NAME_CHARS`; `SessionState.{agent_id, agent_framework}` + `agent_id_or_builtin()` |
| `store` | `AgentProfileRow` / `AgentProfileUpdate` / `AgentProfileStore` port; `StorageError::Conflict` carries duplicate names; `SessionStore::set_agent_binding` (write-once) |
| `storage` | `LibsqlAgentProfileStore` (async `open` seeds the builtin), `agent_profiles` DDL in `init_db()`, `Store.agent_profile` bundle field; `sessions.{agent_id, agent_framework}` columns |
| `gateway` | `api/admin/agents.rs` handlers + DTOs, `AdminState.{agent_profile_store, blob_store}`, the shared `validate_llm_pin`; `api/admin/chat.rs` `resolve_agent_binding` at session creation; `GET /v1/skills` (`?agent_id=` scoped) and `GET /v1/llm/models` feed the read-only skills readout and the model picker |
| `skills` | `SkillRegistry::{load_agent_skills_root, get_scoped, summaries_for_agent}` — the agent-scoped view backing both the runtime overlay and the Agents-page skills readout |
| `agent` | the `LlmPoolHandle` on `AdminState` validates the `llm` pin at write time; `resolve_initial_llm` (session pin > live profile pin > default) at actor spawn/hydration; `ToolContext.agent_id` threading |
| `context` | `ContextManager::try_resolve_system_prompt`'s agent-profile arm (live at seed + post-compaction reseed); `agent_scope()` feeds every skill-listing consumer |
| `memory` | `MemoryContext.agent_id` partition key, sourced from `agent_id_or_builtin()` |
| `web` | `AgentsPage.tsx` (management + scoped skills readout), `AgentPicker.tsx` + session chips (chat consumption), route + `IconRail` entry, blob upload/render reuse |
| `subagent` | none — `SubagentProfile` / `SubagentRegistry` are untouched; subagent sessions are never agent-bound |
