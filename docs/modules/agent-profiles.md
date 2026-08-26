# agent-profiles — user-managed chat personas (`AgentProfile`)

## Overview

Agent profiles are user-managed personas: a named, avatar-carrying row bundling an execution framework (`baybo` / `claude` / `codex`) and an optional LLM pin. The operator creates and edits them from a board's team strip in the web dashboard, and a chat session binds to one at creation.

**The row is half of an agent.** The other half is its persona directory,
holding the `SOUL.md` and `IDENTITY.md` it reads and the `skills/` directory
only it sees. Global agents use `<workspace>/personas/<agent_id>/`; newly
created project agents have a `project-<ULID>` id and use
`<workspace>/personas/project/<agent_id>/`. Older unprefixed project ids remain
at their original flat location. The split is by kind of content: the row
carries what the system queries (a framework, an LLM pin, an avatar blob,
and a board membership), the directory carries the prose the agent itself
rewrites. Neither a prompt nor a skill list is a column. See
[`../todo/multi-agent-chat.md`](../todo/multi-agent-chat.md) for the binding,
the resolution rules, and what is still unbuilt.

This is a cross-crate feature subsystem, not a crate. The pieces live where their kind of code already lives:

- `crates/model/src/agent_profile.rs` — `AgentProfileId`, `AgentFramework`, `BUILTIN_AGENT_PROFILE_ID`, `MAX_AGENT_PROFILE_NAME_CHARS`.
- `crates/store/src/agent_profile.rs` — `AgentProfileRow`, `AgentProfileUpdate`, the `AgentProfileStore` trait.
- `crates/storage/src/sqlite/agent_profile.rs` — `SqliteAgentProfileStore` + the `agent_profiles` table in `init_db()`.
- `crates/gateway/src/api/admin/agents.rs` — the `/v1/agents` handlers and DTOs.
- `app/web/src/pages/projects/TeamStrip.tsx` + `AgentProfile.tsx` — the management surface, inside the board page.

**What this is NOT.** It is not `SubagentProfile` ([`subagent.md`](subagent.md)): that is the filesystem-authoritative registry (`<workspace>/agents/<name>.md`, `DashMap`, disk wins on `reload`) that types *spawned subagents*. Agent profiles are DB-authoritative, web-managed, and aimed at *top-level* sessions. The two registries do not read each other, share no types beyond `baybo-model`, and a name appearing in both means nothing. It is also not the Soul itself: a soul is a file, and the row only names which agent's file to read. The built-in's persona is an ordinary directory, `personas/baybo/`, so an unbound session and a built-in-bound one assemble byte-identical prompts.

## Data model

```rust
// crates/store/src/agent_profile.rs
pub struct AgentProfileRow {
    pub id: AgentProfileId,            // ULID, or project-<ULID>; persona leaf-dir name
    pub description: String,
    pub avatar_blob_id: Option<String>,// full blob id incl. read token, from POST /v1/blobs
    pub framework: AgentFramework,     // baybo | claude | codex
    pub llm: LlmPin,                   // entry + model-within-it + thinking rung; baybo-only
    pub builtin: bool,                 // the seeded `baybo` row; locked except avatar
    pub team: Option<TeamMembership>,  // project_id + @handle; None = a global agent
    pub hired_by: Option<AgentProfileId>, // None = the operator created it
    pub deleted_at: Option<DateTime<Utc>>, // team members tombstone; globals are deleted outright
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

`AgentFramework` is a flat unit enum in `baybo-model` (`#[serde(rename_all = "snake_case")]` + the house `as_str()` / `parse()` / `const ALL` mirror): `Baybo | Claude | Codex`, `Baybo` the `Default`. Its string forms are a subset of the spawn protocol's backend tags — `BAYBO_BACKEND_TAG` and `ExternalAgentKind::as_str()` — and it carries `to_backend_kind(self) -> SubagentBackendKind` so future runtime wiring is a lossless mapping, not a string translation.

`NULL` consistently means **inherit the default**:

| Field | `NULL` / `None` | `Some(...)` |
|---|---|---|
| `llm` | follow `default-llm` | pin to this `baybo.json` entry name |
| `llm_model` | that entry's own `model` | pin to one of its `[model] + model_list` ids |
| `llm_effort` | that entry's own `reasoning_effort` | pin to a rung of baybo's thinking ladder |

The three are one value, `baybo_model::LlmPin`, and one write. Two of them
mean nothing alone — a model id is a model *of an entry*, and a rung is
translated into the entry's provider dialect — so a setter that could move
one without the others would leave a row naming a model the entry it names
cannot serve. `set_llm` takes the whole pin; clearing clears all three.

They exist because a **card's run has no header to pick from**. A chat session
carries its own `last_llm` / `last_model` / `last_effort`, set by the
model picker the operator presses per conversation; a board run's session may
not carry one — see below — so what the profile says is the only thing that
decides what that run costs and how hard it thinks.
Carrying only the entry meant every board agent ran that entry's default model
at that entry's default rung, with no way to say otherwise.

**There is no prompt column, and no name column.** An agent's prompt is its
own persona's `SOUL.md`, and its name is the `Name:` line in that persona's
`IDENTITY.md` — files, so the agent rewrites both through
`Edit` and git keeps the history (see
[`../todo/multi-agent-chat.md`](../todo/multi-agent-chat.md)).

That makes the name **not unique and not sortable in SQL**, which is not a
loss: no constraint could have held one, since a global agent may rename itself
to anything at any moment. The **id** is the identity — every binding, memory
partition, skill directory and API path keys off it — so a duplicate name is a
display ambiguity, never a correctness problem. `list` therefore orders by
`builtin DESC, id` and the gateway re-sorts by the name it reads from each
agent's file.

**A project agent is the exception: its name is fixed at hire.** Its `@handle`
was derived from that name and never moves (see
[`project.md`](project.md#the-team)), so a rename would leave the roster and
every mention, assignment and timeline entry naming different things. The rule
is `baybo_workspace::name::rejected_rename`, keyed on the `project-` id prefix
so it is answerable in the tool layer too.

Four doors reach that line — `PUT …/name`, `PUT …/identity`, and the agent's
own `Edit` and `Write` — and three of four asking would not be a rule at all.
So none of them asks: each crate has exactly **one writer** of an identity
file, and applying the rules is part of writing rather than a step a caller
takes first. `baybo_tools::builtin::managed_repo::write_managed_file` is the
tools' (nothing else in that crate `fs::write`s under `personas/`), and
`replace_identity_file` in `api/admin/agents.rs` is the gateway's (the sole
caller of its `write_file_atomic`). A fifth door gets the rules by
construction; one that skips them has left the tier, losing the audit commit
and the approval bypass with it — not something done by accident.

Losing the line entirely is a second, narrower rule
(`rejected_name_removal`): refused at the two tool doors, where an incidental
reformat could cost the agent its name, and *not* at the whole-file `PUT`,
where a caller who replaces the document means it — restoring the shipped
template leaves it nameless, which is exactly why an unnamed agent has a
defined rendering (its id).

The pin is stored regardless of `framework` (the server never clears it on a framework switch, so switching never destroys data), but it is genuinely baybo-only: it names a `baybo.json` LLM-pool entry, which an external CLI (billed against its own subscription) can't route through, so on a `claude`/`codex` agent the whole pin is inert. The board's panel still offers it — it does not read `framework` — which is a gap worth naming rather than a claim to make: a pin set there is stored and then ignored.

It is read at actor spawn by `resolve_spawn_pins`, behind the session's own pin, and the two levels fall back on **different granularities** on purpose:

- **entry and model fall back together.** A session that named its own entry keeps its own model — an empty one meaning that entry's default — and never inherits a model chosen for a different entry, which is a model that entry cannot serve.
- **the rung falls back on its own** (`last_effort ?? profile.effort`). Effort is a provider-level knob rather than a property of one model, so an agent deliberately set to think hard keeps doing so across a session that only re-pointed which entry to use.

**A card's run session is not behind its own pin.** The precedence above is a *conversation's*: the operator pressed a button meaning "this thread, this model". A run's session is minted per (issue, agent) and reused by every later run of that agent on that card, so a pin on it would outlive the run that set it and outrank every later profile edit — a roster naming one model and runs on another, with nothing on the board saying why. So `TriggerSource::can_pin_its_own_llm` is false for `Issue`, and both ends honour it: `resolve_spawn_pins` returns the profile whole without reading the session's columns, and `PUT /v1/chat/sessions/{id}/model` — which the owner-channel scope check would otherwise admit for a run session — refuses with a 400 naming the agent as the place to change it. Every run therefore starts on whatever the profile says at the moment it spawns; the one thing an edit cannot reach is a run already in flight, whose pin was resolved at its spawn. A value left on such a row by a build that predates the refusal is inert rather than cleared — nothing reads it, and per the no-legacy-migration rule nothing sweeps it either.

**Neither skills nor tools are stored on the profile.** Skills are managed by the skill system, not configured per agent — the editor reads them live from the skill registry (`GET /v1/skills`) and shows them read-only; when a future per-agent-workspace model lands, that readout reads the agent's own skill folder instead. Tools are a runtime-global concern (`ToolRegistry` is process-wide by design) and Claude Code / Codex manage their own tool permissions. Storing either as a per-agent allow-list would be dead data in v1, so both are left out.

### The built-in `baybo` profile

Exactly one row is seeded with `id = BUILTIN_AGENT_PROFILE_ID` (`"baybo"`), `builtin = 1`, `framework = baybo`, every nullable field `NULL`, and its description from the `BUILTIN_AGENT_PROFILE_DESCRIPTION` const in the sqlite impl — the row *is* the default behavior. It cannot be deleted: the agent list always has an honest entry for "the assistant you already have", and the session-binding UI gets a default target without special-casing "no profile".

**Two things about the builtin are fixed by what it *is*:** it runs on `baybo`, and it follows `default-llm`. Everything else is ordinary editable content:

| Field | Builtin | Where it lives |
|---|---|---|
| name | editable (`…/name`) | `personas/baybo/IDENTITY.md`, the `Name:` line |
| soul | editable (`…/soul`) | `personas/baybo/SOUL.md` |
| avatar | editable (`…/avatar`) | row column, via `set_avatar` |
| description | editable (content `PUT`) | row column |
| **framework** | **pinned to `baybo`** | row column |
| **model / llm** | **pinned empty at all three levels** — follows `default-llm` | row columns |

Both pins are structural rather than handler checks. `update` writes `framework = CASE WHEN builtin = 1 THEN framework ELSE ?N END`, so the builtin's column self-references and no caller can move it; `set_llm` writes `CASE WHEN builtin = 1 THEN NULL ELSE ?N END` over each of `llm` / `llm_model` / `llm_effort`, which both refuses a pin and normalises a row an earlier build let drift — a leftover rung is spend the operator chose in a second place, so "cleared" has to mean every level. Pinning a model on the builtin would put one decision — "what does this deployment run on?" — in two places that could then disagree; `default-llm` is that decision, and the LLM page is where it lives.

The gateway still answers an *explicit* attempt with a 400 rather than silently dropping it — a caller that asked deserves to hear no — and refuses before writing, so the rest of the body does not land either. `delete` keeps its plain `WHERE builtin = 0`.

## Design Decisions

### DB-backed, not workspace files

A create materialises the persona directory and writes the name **before** inserting the row: the row is what makes an agent visible and what a retry would duplicate, so a filesystem failure must not leave a half-made one in the roster. An orphaned directory is inert — nothing sweeps `personas/`, by design.

Web CRUD is the primary interface, avatars are binary, and edits are concurrent — that is the profile of the sqlite-managed entities (`sessions`, `session_folders`, `cron_jobs`), not of the git-versioned workspace markdown (`personas/`, subagent `agents/`). The DB row is the single source of truth; there is no file mirror, no watcher, no `reload()` semantics.

### The builtin row is locked structurally, not by convention

`AgentProfileStore::update` and `::delete` execute with `WHERE id = ?1 AND builtin = 0`, so a builtin mutation affects zero rows no matter who calls the store — the gateway's crisp 400 is UX on top, not the enforcement. `set_avatar` is the one write without the guard, because the avatar is the one field the builtin allows. The `builtin` column is written only by the seed: `create`'s `INSERT` omits the column (schema `DEFAULT 0` fills it), so a second `builtin = 1` row — which the guards would make permanently un-editable and un-deletable — is unmintable through the store.

### Seeding is `INSERT OR IGNORE` at store open

`SqliteAgentProfileStore::open(pool)` (async, mirroring `SqliteBlobStore::open`) runs the builtin seed after `init_db()`: `INSERT OR IGNORE` with the fixed id, so a fresh DB gets the row and an existing DB keeps it — including a user-set avatar, because ignore-on-conflict never touches the live row. `init_db()` itself stays pure DDL, and there is no boot-time re-assert of the other builtin fields: no write path can reach them.

### The `llm` pin is validated at write, tolerated at read

The whole pin is validated against the live pool on write, by the shared `validate_llm_pin` — the **single home** of that rule, which the session model switch, the profile pin and the hire form all come through, so what a client may send cannot drift between them. Three checks, each rejecting rather than degrading: an unknown entry, a model that is not one of that entry's `[model] + model_list` (or a model sent with no entry at all, which means nothing), and a rung outside baybo's ladder. A crisp 400 at edit time beats a silent fallback, because every one of these would otherwise be discarded at run time while the UI kept showing the choice. The rung is canonicalised on the way in, so `none` and `off` cannot persist as two spellings of one rung.

Staleness *later* is tolerated the same way `sessions.last_llm` staleness is — an entry removed from `baybo.json`, a model dropped from a `model_list`, a rung a provider stopped expressing: the pool's `resolve` falls back to default with a `warn!` when consumption arrives, and the editor renders a stale pick as "(unavailable)" at whichever level it went stale, so it stays visible and clearable. One helper in `teamModel.ts` answers that for all three pickers.

Note the two sources are not the same: the pickers are filled from `GET /v1/llm/models`, which reads the **config on disk**, while `validate_llm_pin` checks the **live pool**. An entry whose client failed to build is therefore offered and then refused — which is the right direction (visible failure, not a silent downgrade), and why the panel keeps the 400 on screen.

### Full-replace `PUT`, targeted avatar endpoint

Nullable fields where `NULL` is meaningful make a partial `PATCH` need absent-vs-null tri-state, which serde/utoipa express badly. So content updates are a **full replace**: the body carries the complete content state — `name`/`description`/`framework` **required** (so an omitted `framework` can't silently reset a profile to `baybo`), absent nullable fields reset to `NULL`. Racing `PUT`s are last-write-wins, no version precondition. The avatar is excluded and lives on its own targeted endpoint (house style: `…/model`, `…/pin`, `…/folder`) — that is what maps "builtin is locked except avatar" onto endpoints instead of per-field conditional validation.

### Identity

`id` is minted server-side: global agents use `AgentProfileId::generate()` (a ULID — the `FolderId` pattern), while project-owned agents use `AgentProfileId::generate_project()` (`project-<ULID>`). Unlike `FolderId`, it is **not** opaque: it becomes the persona directory's leaf name, and the project prefix also selects the grouped tree. It therefore carries the skill-name grammar enforced by `AgentProfileId::parse` and by a validating `Deserialize`; the exact id `project` is reserved for the container directory (a guard only on the constructor would be bypassed by every request body and stored row that parses one).

The **name** is not stored. `POST` / `PUT /v1/agents` still accept one — free-form after trim, non-empty, at most `MAX_AGENT_PROFILE_NAME_CHARS` (64) characters — but the handler splices it into the `Name:` line of that agent's `IDENTITY.md` rather than a column, preserving every other line the agent wrote. Reads derive it back the same way, falling back to the id when the file carries no usable name (the shipped template's state — it invites the agent to choose). Editing the name is therefore the same operation whether the operator or the agent does it, and there is nothing to keep in sync — which is also what lets the fixed-name rule above be enforced in one place for both.

That template is the *global* one. A project agent is seeded from `PROJECT_PERSONA_IDENTITY_TEMPLATE` instead, whose `Name:` line says the board set it and that the `@handle` came from it — the fields are otherwise identical. A file that invited the agent every turn to pick a name the tools then refuse to change would be a prompt bug, not a harmless nicety.

### Avatars ride the existing blob pipeline

No new image storage: the client uploads via the existing `POST /v1/blobs` and stores the returned `blob_id` — the full `sha256:<digest>.<read-token>` capability string — on the profile. Every path that persists an `avatar_blob_id` (`PUT …/avatar` **and** a `POST` create that supplies one) `stat`s the blob first and 400s on an unknown id or a stored mime outside `image/*`; error messages redact the read token. After that the reference is soft (FKs are off workspace-wide). No avatar-specific size cap — the blob pipeline's `MAX_BLOB_BYTES` is the only bound, deliberately. Replaced avatars orphan their old blob **inert** — per the repo rule, no cleanup sweeper.

## Storage

```sql
CREATE TABLE IF NOT EXISTS agent_profiles (
    id              TEXT PRIMARY KEY,
    description     TEXT NOT NULL,
    avatar_blob_id  TEXT,
    framework       TEXT NOT NULL,          -- AgentFramework::as_str()
    llm             TEXT,                   -- one column per LlmPin level;
    llm_model       TEXT,                   -- NULL inherits at each, and the
    llm_effort      TEXT,                   -- three are only written together
    builtin         INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,       -- Unix µs (sqlite/time.rs)
    updated_at      INTEGER NOT NULL
);
```

House encodings throughout: timestamps µs via `super::time`, booleans `INTEGER` 0/1 (an unknown `framework` string on read is an error, never a silent fallback). No `skills` column — skills are read live from the registry, not stored per agent — and no prompt column, for the same reason one level up: the soul is a file.

```rust
// crates/store/src/agent_profile.rs
#[async_trait]
pub trait AgentProfileStore: Send + Sync {
    async fn list(&self) -> Result<Vec<AgentProfileRow>>;            // global agents only; ORDER BY builtin DESC, id
    async fn list_team(&self, project: &ProjectId) -> Result<Vec<AgentProfileRow>>;         // live roster; ORDER BY handle
    async fn list_team_history(&self, project: &ProjectId) -> Result<Vec<AgentProfileRow>>; // tombstones included, oldest first
    async fn get(&self, id: &AgentProfileId) -> Result<Option<AgentProfileRow>>;            // reaches removed team members
    async fn create(&self, row: &AgentProfileRow) -> Result<()>;     // duplicate id → Conflict; never binds `builtin`
    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool>; // reaches the builtin, never its framework
    async fn set_avatar(&self, id: &AgentProfileId, blob_id: Option<&str>) -> Result<bool>;   // builtin allowed
    async fn set_llm(&self, id: &AgentProfileId, pin: &LlmPin) -> Result<bool>;             // whole pin; never the builtin
    async fn delete(&self, id: &AgentProfileId) -> Result<bool>;     // WHERE builtin = 0 AND project_id IS NULL; plain row DELETE
    async fn remove_from_team(&self, id: &AgentProfileId) -> Result<bool>;                  // stamps `deleted_at`
}
```

`AgentProfileUpdate` is the row's remaining content state minus `id`/`avatar_blob_id`/`builtin`/`team`/`hired_by`/`deleted_at`/timestamps (so: description, framework); `update`, `set_llm` and `set_avatar` bump `updated_at`. `project_id`/`handle` are absent from every one of these, and that is enforced by the schema rather than by this list staying short: an `agent_profiles_team_is_insert_only` trigger aborts any `UPDATE` that moves either (see [`storage.md`](storage.md)). It is the store-side twin of the fixed name above — the same identity, split across a column SQL can guard and a file it cannot. No write can conflict on content — the one `UNIQUE` column went away with `name`. `Ok(false)` = no row matched (missing id, or the builtin behind the guard) — the gateway `get`s first to disambiguate, and reads `Ok(false)` after a non-builtin `get` as a concurrent delete → 404. The store stays a dumb writer — name/llm/blob validation lives in the gateway handlers — and rides the `Store.agent_profile` bundle field out of `Store::open`.

## HTTP API

Route module `crates/gateway/src/api/admin/agents.rs`, tag `agents`, DTOs inline per the folders precedent. `AgentProfileDto` mirrors the row — `framework` as an enum mirror, timestamps RFC3339, nullable fields skipped when `None`. No `skills` field on any agent DTO (create, update, or response); the editor sources skills from the separate `GET /v1/skills`.

| Endpoint | Success | Errors |
|---|---|---|
| `GET /v1/agents` | 200 `ListResponse<AgentProfileDto>` | 401 |
| `POST /v1/agents` | 200 `AgentProfileDto` | 400 name invalid/duplicate, bad llm pin, unknown/non-image `avatar_blob_id`; 401 |
| `GET /v1/agents/{agent_id}` | 200 `AgentProfileDto` | 401, 404 |
| `PUT /v1/agents/{agent_id}` | 204 | 400 builtin locked, name invalid/duplicate; 401, 404 |
| `PUT /v1/agents/{agent_id}/model` | 204 | 400 builtin locked, unknown entry / model not of that entry / model without an entry / unknown rung; 401, 404 |
| `PUT /v1/agents/{agent_id}/avatar` | 204 | 400 unknown blob id / non-image mime; 401, 404 |
| `DELETE /v1/agents/{agent_id}` | 204 | 400 builtin; 401, 404 |

- `POST` body = full content state (`name` required; `description` defaults empty; `framework` defaults `baybo`; nullable fields absent = `NULL`) **plus** optional `avatar_blob_id` — create can't hit the builtin lock, so no avatar asymmetry is needed there. `PUT` body = the same shape minus `avatar_blob_id`, with `name`/`description`/`framework` required.
- `PUT …/avatar` body = `{ "blob_id": "sha256:…" | null }`; `null` clears the avatar.
- `PUT …/model` body = `{ llm, model, reasoning_effort }`, each nullable — the **whole pin, replaced as one**, deliberately named the same as `SetSessionModelRequest` so the two write surfaces read identically. An empty body clears the pin entirely rather than leaving two thirds of it pointing at an entry the agent no longer uses. The same three fields are optional on `POST /v1/agents` and on the board's `POST /v1/projects/{project_id}/agents`, so a hire can be staffed in one call.
- Builtin refusals are `GatewayError::BadRequest` (the admin surface's "operation not allowed" convention — there is no `Forbidden` variant), with the store's `builtin = 0` guard as backstop.
- `AdminState` carries `agent_profile_store` and `blob_store` (for the avatar `stat`), both off `deps.stores`.

Shape changes ride the standard openapi regen chain — see the header of `crates/gateway/src/api/dto.rs`.

## Web UI

There is no standalone Agents page and no `/agents` route: the surface lives inside the board page, as `TeamStrip.tsx` (the roster strip plus the hire form, over `POST /v1/projects/{project_id}/agents`) and the `AgentProfile.tsx` panel it opens (per-agent detail, and the LLM pin via `PUT /v1/agents/{agent_id}/model`). Both draw the pin with the same `LlmPinFields.tsx` — one component, because both must offer the same three rows under the same rules, and those rules go wrong quietly when written twice. Both follow the house conventions (hand-rolled fetch with refetch-after-mutation, `useAdminClient()`, 401 → `logout()`, `?mock=true` with every mutation short-circuited in mock mode). The panel is a `FloatingPanel` over the board (one per agent, keyed by id), not a page of its own — it is also mounted from `ColumnPage`.

- **Roster strip**: one avatar per teammate (uploaded blob, else a deterministic generated face from `botttsFace(agent_id)`), tooltipped `name (@handle)` + run note + description, capped at `MAX_AGENTS = 16` mirroring the server's `MAX_TEAM_AGENTS`; a `+` button opens the hire form (name, role/description, framework picker — `native` / `claude` / `codex`), which `POST`s to `/v1/projects/{project_id}/agents`.
- **Profile panel**: opened from the strip, keyed by agent. Header `@handle`, then description, `framework (fixed at hire)`, who hired it (`— hired by @x`, flagged `(since removed)` when the hirer has left), the LLM pin as the three `Picker`s of `LlmPinFields` — entry, the model within it, and thinking — writing the whole triple with one `PUT /v1/agents/{agent_id}/model`, and a read-only skills readout ("the shared set, in v1") since skills aren't per-profile. Footer: **Remove from project** behind an inline confirm, hidden for a read-only board and for the lead.
- **Avatar**: rendered, not uploaded. `useTeamPortraits` fetches each agent's blob once per board with the bearer into an object URL (an `<img>` can't carry the auth header) and falls back to the generated face; no web path calls `PUT /v1/agents/{agent_id}/avatar`, which stays API-only.
- **Live over WS**: the roster refetches on `Frame::ProjectChanged` for this board — `useBoardStream` bumps the `refreshKey` the board's `fetchTeam` effect is keyed on — so a hire or a removal made elsewhere lands without a reload.

## Constraints

- Feature subsystem, not a crate: no `baybo-agent-profiles` crate until there is behavior beyond CRUD. Domain types in `model`, port in `store`, impl in `storage`, policy in `gateway` handlers.
- **The row is not the agent.** `ContextManager` reads the resolved persona,
  skills, and memory partition keyed by the id the session carries, none of
  them by the row. So deleting a row strands nothing: the conversation keeps
  the persona, the skills and the memories it has been talking to, and only
  the row's own fields (the llm pin, the roster entry) go.
- Strictly disjoint from `SubagentProfile` and from the workspace Soul; the only shared vocabulary is `baybo-model` (`ExternalAgentKind`, `LlmEntryName`, the backend tag strings).
- All cross-entity references are soft (FKs are off): `avatar_blob_id` into
  `blobs`, `llm` into `baybo.json`. Write-time validation where it's cheap and
  crisp (llm, avatar), tolerance at read time everywhere. Deleting a row does
  not touch its resolved persona directory, and nothing in the persona path
  consults the row — so a bound conversation keeps the agent it has been
  talking to, the same way it keeps that agent's memories.
- `name` carries an explicit length bound (`MAX_AGENT_PROFILE_NAME_CHARS`) **and** a round-trip check — it is rejected unless `display_name(with_display_name(…))` returns it unchanged, so a create response and the next `GET` can never disagree about what the agent is called. Identity-file writes carry a 1 MiB cap enforced by the API and by the `Edit` tool alike. `description` is deliberately bounded only by the admin request-body limit.
- Profile rows are user data with a normal delete affordance — the session never-delete rule does not apply — but there is still no background sweeper of any kind, and orphaned avatar blobs stay inert.

## Deferred

Session binding, the per-agent persona directory, and the memory partition
ship in [`../todo/multi-agent-chat.md`](../todo/multi-agent-chat.md), which is
authoritative for all three. What is left:

- **External-framework top-level sessions** — `claude`/`codex` currently run only as subagent backends; a profile with an external framework needs the external-agent leg generalized to top-level chat.
- **Markdown export/import** — if git-versioning of personas ever matters; DB stays authoritative.
- **@-mention / slash selection** — unique names are already reserved for it.

## Collaboration

| Module | Role |
|---|---|
| `model` | `AgentProfileId` (ULID / `project-<ULID>` string newtype), `AgentFramework` (+ `to_backend_kind`), `BUILTIN_AGENT_PROFILE_ID`, `MAX_AGENT_PROFILE_NAME_CHARS` |
| `store` | `AgentProfileRow` / `AgentProfileUpdate` / `AgentProfileStore` port; `StorageError::Conflict` carries duplicate names |
| `storage` | `SqliteAgentProfileStore` (async `open` seeds the builtin), `agent_profiles` DDL in `init_db()`, `Store.agent_profile` bundle field |
| `gateway` | `api/admin/agents.rs` handlers + DTOs, `AdminState.{agent_profile_store, blob_store}`, the shared `validate_llm_pin`; `GET /v1/skills` (`{name, description, universal}` from `SkillRegistry::summaries_for(agent_id)`) and `GET /v1/llm/models` feed the read-only skills readout and the model picker |
| `skills` | `SkillRegistry::summaries_for(agent_id)` is the live source rendered read-only in the skills readout — per-agent scoped, so the readout is the agent's own set, not the whole registry |
| `agent` | the `LlmPoolHandle` on `AdminState` validates the `llm` pin at write time |
| `web` | `projects/TeamStrip.tsx` + `projects/AgentProfile.tsx` on the board page, `projects/portrait.ts` avatar-blob rendering (`botttsFace` fallback); no avatar *upload* path in the web client — `PUT /v1/agents/{agent_id}/avatar` is API-only |
| `subagent` / `context` | **none in v1** — first coupling arrives with session binding (Deferred) |
