# agent-profiles — user-managed chat personas (`AgentProfile`)

## Overview

Agent profiles are user-managed personas: a named, avatar-carrying bundle of system prompt, an execution framework (`baybo` / `claude` / `codex`), and an optional LLM pin. The operator creates and edits them from the web dashboard; a later feature will let a chat session bind to one so the profile drives that session's behavior. Skills are **not** a profile field — the editor displays them read-only, live from the skill registry (managed by the skill system, not configured per agent here).

**v1 is management-only.** The feature ships the entity, its persistence, full CRUD over `/v1/agents`, and the web **Agents** page — and deliberately **no runtime consumer**: nothing in `baybo-agent`, `baybo-context`, or the spawn path reads `agent_profiles` yet. Binding, allow-list enforcement, and external-framework top-level sessions are all in [Deferred](#deferred).

This is a cross-crate feature subsystem, not a crate. The pieces live where their kind of code already lives:

- `crates/model/src/agent_profile.rs` — `AgentProfileId`, `AgentFramework`, `BUILTIN_AGENT_PROFILE_ID`, `MAX_AGENT_PROFILE_NAME_CHARS`.
- `crates/store/src/agent_profile.rs` — `AgentProfileRow`, `AgentProfileUpdate`, the `AgentProfileStore` trait.
- `crates/storage/src/sqlite/agent_profile.rs` — `SqliteAgentProfileStore` + the `agent_profiles` table in `init_db()`.
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

`llm` is stored regardless of `framework` (the server never clears it on a framework switch, so switching never destroys data), but it is genuinely baybo-only: it names a `baybo.json` LLM-pool entry, which an external CLI (billed against its own subscription) can't route through — the editor greys it out for external frameworks. Nothing is consumed yet (v1 is management-only).

**Neither skills nor tools are stored on the profile.** Skills are managed by the skill system, not configured per agent — the editor reads them live from the skill registry (`GET /v1/skills`) and shows them read-only; when a future per-agent-workspace model lands, that readout reads the agent's own skill folder instead. Tools are a runtime-global concern (`ToolRegistry` is process-wide by design) and Claude Code / Codex manage their own tool permissions. Storing either as a per-agent allow-list would be dead data in v1, so both are left out.

### The built-in `baybo` profile

Exactly one row is seeded with `id = BUILTIN_AGENT_PROFILE_ID` (`"baybo"`), `name = "baybo"`, `builtin = 1`, `framework = baybo`, every nullable field `NULL`, and its description from the `BUILTIN_AGENT_PROFILE_DESCRIPTION` const in the sqlite impl — the row *is* the default behavior. It is **read-only except its avatar** and cannot be deleted: the agent list always has an honest entry for "the assistant you already have", and a future session-binding UI gets a default target without special-casing "no profile".

## Design Decisions

### DB-backed, not workspace files

Web CRUD is the primary interface, avatars are binary, and edits are concurrent — that is the profile of the sqlite-managed entities (`sessions`, `session_folders`, `cron_jobs`), not of the git-versioned workspace markdown (Soul `profile/`, `skills/`, subagent `agents/`). The DB row is the single source of truth; there is no file mirror, no watcher, no `reload()` semantics.

### The builtin row is locked structurally, not by convention

`AgentProfileStore::update` and `::delete` execute with `WHERE id = ?1 AND builtin = 0`, so a builtin mutation affects zero rows no matter who calls the store — the gateway's crisp 400 is UX on top, not the enforcement. `set_avatar` is the one write without the guard, because the avatar is the one field the builtin allows. The `builtin` column is written only by the seed: `create`'s `INSERT` omits the column (schema `DEFAULT 0` fills it), so a second `builtin = 1` row — which the guards would make permanently un-editable and un-deletable — is unmintable through the store.

### Seeding is `INSERT OR IGNORE` at store open

`SqliteAgentProfileStore::open(pool)` (async, mirroring `SqliteBlobStore::open`) runs the builtin seed after `init_db()`: `INSERT OR IGNORE` with the fixed id, so a fresh DB gets the row and an existing DB keeps it — including a user-set avatar, because ignore-on-conflict never touches the live row. `init_db()` itself stays pure DDL, and there is no boot-time re-assert of the other builtin fields: no write path can reach them.

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
    created_at      INTEGER NOT NULL,       -- Unix µs (sqlite/time.rs)
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
- **Detail**: an inline character-sheet editor (no modal, centered single column), keyed by selection. Header: large avatar portrait + image/remove controls beside the name field, a fixed-min-height meta row (builtin lock badge / id) so switching agents never jumps the layout. Body: description, framework, LLM pin, system prompt, then a full-width **read-only skills readout** — every registered skill listed with its `SKILL.md` description (from `GET /v1/skills`, which returns `{name, description}`), the same live registry for every agent since skills aren't per-profile in v1. A removed LLM entry renders as "(unavailable)"; the LLM pin greys out for external frameworks ("baybo only"). Footer: destructive Delete on the left (custom agents; a confirm dialog follows), Save on the right. For the builtin everything except the avatar is disabled and Save is live only when the avatar changed.
- **Avatar**: file input → `POST /v1/blobs` → `PUT /v1/agents/{agent_id}/avatar`; rendering fetches the blob with the bearer into an object URL (an `<img>` can't carry the auth header). The bundled builtin default is `app/web/src/assets/baybo-avatar.webp` (a 256² webp squeezed from `assets/baybo.png`; `avatar_blob_id` stays `NULL`).
- **No WS**: nothing consumes agent profiles live — no `Frame` variant, no store/context. A `FolderView`-style broadcast can be added when binding lands.

## Constraints

- Feature subsystem, not a crate: no `baybo-agent-profiles` crate until there is behavior beyond CRUD. Domain types in `model`, port in `store`, impl in `storage`, policy in `gateway` handlers.
- **No runtime coupling in v1.** `ContextManager`, `AgentLoop`, the spawn router, and `SubagentRegistry` are untouched. Deleting a custom profile can strand nothing, because nothing references profiles yet.
- Strictly disjoint from `SubagentProfile` and from the workspace Soul; the only shared vocabulary is `baybo-model` (`ExternalAgentKind`, `LlmEntryName`, the backend tag strings).
- All cross-entity references are soft (FKs are off): `avatar_blob_id` into `blobs`, `llm` into `baybo.json`. Write-time validation where it's cheap and crisp (llm, avatar), tolerance at read time everywhere.
- Only `name` carries an explicit length bound (`MAX_AGENT_PROFILE_NAME_CHARS`); `description` and `system_prompt` are deliberately bounded only by the admin request-body limit.
- Profile rows are user data with a normal delete affordance — the session never-delete rule does not apply — but there is still no background sweeper of any kind, and orphaned avatar blobs stay inert.

## Deferred

The first three items below are designed as one feature in
[`../todo/multi-agent-chat.md`](../todo/multi-agent-chat.md) — session binding,
a per-agent persona directory (`personas/<agent_id>/` carrying `SOUL.md` and a
`skills/` overlay), a memory partition per agent, and external-framework chat.
That spec is authoritative where it differs from the sketches here: the binding
is INSERT-seeded and immutable rather than a targeted setter, and an agent's
prompt is its own `SOUL.md` rather than the `system_prompt` column.

- **Session binding** — the consumer this schema was shaped for: a flat anti-clobber `sessions.agent_id` column with a targeted setter, `PUT /v1/chat/sessions/{session_id}/agent` persisting then live-re-pinning via an `AgentMessage` (the exact `last_llm` split), and `ContextManager` resolving the bound profile's prompt with `NULL` → Soul.
- **Per-agent skills** — the envisioned model is each agent owning a workspace folder with its own skills (like Claude Code's `.claude/skills/`), discovered live. The read-only skills readout would then read that agent's folder instead of the global registry. This is why skills are deliberately *not* a stored profile allow-list.
- **External-framework top-level sessions** — `claude`/`codex`/`gemini` currently run only as subagent backends; a profile with an external framework needs the external-agent leg generalized to top-level chat. (`gemini` exists as a runtime backend but isn't offered as an agent-profile framework.)
- **Markdown export/import** — if git-versioning of personas ever matters; DB stays authoritative.
- **@-mention / slash selection** — unique names are already reserved for it.

## Collaboration

| Module | Role |
|---|---|
| `model` | `AgentProfileId` (ULID-minted string newtype), `AgentFramework` (+ `to_backend_kind`), `BUILTIN_AGENT_PROFILE_ID`, `MAX_AGENT_PROFILE_NAME_CHARS` |
| `store` | `AgentProfileRow` / `AgentProfileUpdate` / `AgentProfileStore` port; `StorageError::Conflict` carries duplicate names |
| `storage` | `SqliteAgentProfileStore` (async `open` seeds the builtin), `agent_profiles` DDL in `init_db()`, `Store.agent_profile` bundle field |
| `gateway` | `api/admin/agents.rs` handlers + DTOs, `AdminState.{agent_profile_store, blob_store}`, the shared `validate_llm_pin`; `GET /v1/skills` (now `{name, description}` from `all_summaries_sorted`) and `GET /v1/llm/models` feed the read-only skills readout and the model picker |
| `skills` | `SkillRegistry::all_summaries_sorted()` is the live source rendered read-only in the skills readout |
| `agent` | the `LlmPoolHandle` on `AdminState` validates the `llm` pin at write time |
| `web` | `AgentsPage.tsx`, route + `IconRail` entry, blob upload/render reuse |
| `subagent` / `context` | **none in v1** — first coupling arrives with session binding (Deferred) |
