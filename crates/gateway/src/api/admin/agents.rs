//! `/v1/agents` — CRUD for user-managed agent profiles (chat personas).
//!
//! Management-only in v1: no runtime consumer reads profiles yet. The
//! seeded built-in profile is read-only except its avatar and cannot be
//! deleted; content updates are a full replace, the avatar rides its own
//! targeted endpoint. See `docs/modules/agent-profiles.md`.

use axum::Json;
use axum::extract::{Path, State};
use baybo_model::{AgentFramework, AgentProfileId, MAX_AGENT_PROFILE_NAME_CHARS};
use baybo_store::StorageError;
use baybo_store::agent_profile::{AgentProfileRow, AgentProfileUpdate};
use baybo_workspace::IdentityKind;
use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};
use tracing::debug;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::{ErrorBody, ListResponse};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_agents))
        .routes(routes!(create_agent))
        .routes(routes!(get_agent))
        .routes(routes!(update_agent))
        .routes(routes!(set_agent_avatar))
        .routes(routes!(set_agent_name))
        .routes(routes!(set_agent_model))
        .routes(routes!(get_agent_soul))
        .routes(routes!(set_agent_soul))
        .routes(routes!(get_agent_identity))
        .routes(routes!(set_agent_identity))
        .routes(routes!(delete_agent))
}

// ── DTOs ────────────────────────────────────────────────────────────

/// Mirror of [`baybo_model::AgentFramework`]; wire strings match the
/// spawn protocol's backend tags (`baybo`/`claude`/`codex`).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentFrameworkDto {
    #[default]
    Baybo,
    Claude,
    Codex,
}

impl From<AgentFramework> for AgentFrameworkDto {
    fn from(f: AgentFramework) -> Self {
        match f {
            AgentFramework::Baybo => Self::Baybo,
            AgentFramework::Claude => Self::Claude,
            AgentFramework::Codex => Self::Codex,
        }
    }
}

impl From<AgentFrameworkDto> for AgentFramework {
    fn from(f: AgentFrameworkDto) -> Self {
        match f {
            AgentFrameworkDto::Baybo => Self::Baybo,
            AgentFrameworkDto::Claude => Self::Claude,
            AgentFrameworkDto::Codex => Self::Codex,
        }
    }
}

/// One agent profile. Absent `llm` = follow `default-llm`.
///
/// Neither the soul nor the skills are fields here. An agent's soul is its
/// own `SOUL.md` (`GET`/`PUT /v1/agents/{agent_id}/soul`) and its skills are
/// read live from the registry (`GET /v1/skills?agent_id=`) — both are
/// files, so they stay editable by hand, by git, and by the agent itself.
#[derive(Debug, Serialize, ToSchema)]
pub struct AgentProfileDto {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_blob_id: Option<String>,
    pub framework: AgentFrameworkDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<String>,
    /// The seeded built-in profile: read-only except its avatar,
    /// cannot be deleted.
    pub builtin: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl AgentProfileDto {
    /// Build the wire shape, pairing the row with the display name read out
    /// of that agent's own `IDENTITY.md`.
    fn from_parts(r: AgentProfileRow, name: String) -> Self {
        Self {
            id: r.id.into_inner(),
            name,
            description: r.description,
            avatar_blob_id: r.avatar_blob_id,
            framework: r.framework.into(),
            llm: r.llm.map(|l| l.to_string()),
            builtin: r.builtin,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// The name to show for an agent whose `IDENTITY.md` has no usable `Name:`
/// line — the shipped template's state, since it invites the agent to
/// choose one. The id is stable, unique and already the thing every other
/// surface keys off, so it beats a placeholder that several agents share.
fn fallback_display_name(row: &AgentProfileRow) -> String {
    row.id.as_str().to_owned()
}

/// Read one agent's chosen name from its own `IDENTITY.md`.
///
/// A file that is missing, unreadable, or has been reformatted past
/// recognition falls back rather than failing the request: the roster must
/// still render, and the agent owns that file.
pub(super) async fn read_display_name(state: &AdminState, row: &AgentProfileRow) -> String {
    let path = row
        .id
        .identity_file(&state.workspace_paths, IdentityKind::Identity);
    match tokio::fs::read_to_string(&path).await {
        Ok(body) => baybo_workspace::display_name(&body).unwrap_or_else(|| {
            debug!(agent_id = %row.id, "agent IDENTITY.md carries no name; showing its id");
            fallback_display_name(row)
        }),
        Err(e) => {
            debug!(agent_id = %row.id, error = %e, "agent IDENTITY.md unreadable; showing its id");
            fallback_display_name(row)
        }
    }
}

/// Rewrite just the `Name:` line of an agent's `IDENTITY.md`, seeding the
/// file first if it does not exist yet.
///
/// A read-modify-write on a file the agent also edits. It is deliberately
/// *not* conditional: the caller renaming an agent is expressing intent about
/// one field, not replacing the document, and the splice preserves every
/// other line — so unlike a whole-file `PUT` it cannot delete what the agent
/// wrote around it.
///
/// The naming rule reaches it through [`replace_identity_file`], which is the
/// one writer both this and the whole-file `PUT` go through.
async fn set_display_name(state: &AdminState, row: &AgentProfileRow, name: &str) -> Result<()> {
    let path = row
        .id
        .identity_file(&state.workspace_paths, IdentityKind::Identity);
    let _guard = identity_write_lock(&path).await;
    let current = baybo_workspace::load_identity(baybo_workspace::IdentitySource::new(
        &path,
        IdentityKind::Identity.default_content(),
    ))
    .await
    .map_err(|e| GatewayError::Internal(format!("read agent identity: {e}")))?;
    let updated = baybo_workspace::with_display_name(&current, name);
    if updated == current {
        return Ok(());
    }
    replace_identity_file(
        state,
        row,
        IdentityKind::Identity,
        &path,
        &current,
        &updated,
    )
    .await
}

/// Put `content` in one of an agent's identity files, after whatever governs
/// that file's contents has passed, and record it in the persona repo.
///
/// **The** gateway write path for these files, for the same reason
/// `baybo-tools` has exactly one: a rule each endpoint remembers to ask about
/// before its own write is a rule the third endpoint will not ask about. Here
/// the check is not a step a handler takes first — it is part of writing.
///
/// `current` is what is on disk *now*, read under the caller's
/// [`identity_write_lock`]: the rules are about the change, and a caller
/// re-reading here would be reading outside its own compare-and-set.
async fn replace_identity_file(
    state: &AdminState,
    row: &AgentProfileRow,
    kind: IdentityKind,
    path: &std::path::Path,
    current: &str,
    content: &str,
) -> Result<()> {
    // Only the rename half. A caller replacing the whole document may leave
    // it without a name — restoring the shipped template does exactly that —
    // which is why an unnamed agent has a defined rendering at all. Losing
    // the line by accident is the *tools'* risk, and their writer refuses it.
    if kind == IdentityKind::Identity
        && let Some(reason) = baybo_workspace::rejected_rename(row.id.as_str(), current, content)
    {
        return Err(GatewayError::BadRequest(reason));
    }
    write_file_atomic(path, content)
        .await
        .map_err(|e| GatewayError::Internal(format!("write agent {kind:?} file: {e}")))?;
    commit_persona_edit(state, row.id.as_str(), kind.file_name()).await;
    Ok(())
}

/// Serializes this process's writers of one identity file.
///
/// Every gateway write to these files is read-modify-write — a compare-and-set
/// on the whole-file `PUT`, a splice-one-line for a rename — and without a
/// lock two of them interleave: both read the same base, both pass, and the
/// second silently discards the first. Keyed by path, so unrelated agents
/// never wait on each other.
///
/// Scope worth stating: it covers the **gateway's** writers. The `Edit` tool
/// writes these files too, guarded by its own read-before-edit rule rather
/// than this lock, so an agent self-edit interleaving with a dashboard save
/// remains last-write-wins — recoverable from the persona repo's history,
/// which is one of the reasons that history exists.
async fn identity_write_lock(path: &std::path::Path) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: std::sync::LazyLock<
        parking_lot::Mutex<
            std::collections::HashMap<std::path::PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>,
        >,
    > = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let lock = {
        let mut locks = LOCKS.lock();
        std::sync::Arc::clone(
            locks
                .entry(path.to_path_buf())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(()))),
        )
    };
    lock.lock_owned().await
}

/// Commit an operator's dashboard edit into the `personas/` repo.
///
/// Not optional bookkeeping: the `Edit` tool commits with a pathspec, which
/// picks up whatever is in the working tree at that path. An operator write
/// left uncommitted is therefore absorbed into the agent's *next* audit
/// commit, attributed to the agent. The history exists to tell those two
/// apart, so each writer commits its own work. Best-effort, like every other
/// write to that repo.
async fn commit_persona_edit(state: &AdminState, agent_id: &str, file: &str) {
    // The pathspec is what git stages, so it must name the single file this
    // request wrote. Passing the agent's directory swept anything else dirty
    // there — including an edit the agent had not committed yet — into a
    // commit attributed to the operator, which is the attribution this
    // history exists to keep straight.
    let spec = format!("{agent_id}/{file}");
    baybo_workspace::commit_personas(
        &state.workspace_paths,
        &[spec.as_str()],
        &format!("personas: operator edited {agent_id} {file}"),
    )
    .await;
}

/// [`AgentProfileDto`] for one row, with its name read from disk.
pub(super) async fn agent_dto(state: &AdminState, row: AgentProfileRow) -> AgentProfileDto {
    let name = read_display_name(state, &row).await;
    AgentProfileDto::from_parts(row, name)
}

/// Request body for `POST /v1/agents`. Absent nullable fields mean
/// "inherit the default" (see [`AgentProfileDto`]).
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAgentProfileRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub framework: AgentFrameworkDto,
    /// Initial soul body. Written once into `personas/<id>/SOUL.md`;
    /// absent seeds the shipped template. Later edits go through
    /// `PUT /v1/agents/{agent_id}/soul` — this field is a convenience for
    /// creating an agent in one call, not a second source of truth.
    #[serde(default)]
    pub soul: Option<String>,
    /// `baybo.json` LLM entry name; must match a configured entry — see
    /// `GET /v1/llm/models`.
    #[serde(default)]
    pub llm: Option<String>,
    /// Optional avatar (full blob id from `POST /v1/blobs`); validated
    /// exactly like `PUT /v1/agents/{agent_id}/avatar`.
    #[serde(default)]
    pub avatar_blob_id: Option<String>,
}

/// Request body for `PUT /v1/agents/{agent_id}` — the **complete** content
/// state (full replace). `name`/`description`/`framework` are required so
/// an omitted `framework` can't silently reset a profile to `baybo`;
/// absent nullable fields reset to the inherit-default state.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateAgentProfileRequest {
    pub description: String,
    pub framework: AgentFrameworkDto,
}

/// Request body for `PUT /v1/agents/{agent_id}/name`.
///
/// Rejected for an agent on a project team: the `@handle` its board addresses
/// it by was derived from its name when it was hired, so the name is fixed for
/// as long as the agent exists.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetAgentNameRequest {
    pub name: String,
}

/// Request body for `PUT /v1/agents/{agent_id}/model`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetAgentModelRequest {
    /// `baybo.json` LLM entry name, or `null`/absent to follow `default-llm`.
    #[serde(default)]
    pub llm: Option<String>,
}

/// Request body for `PUT /v1/agents/{agent_id}/avatar`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetAgentAvatarRequest {
    /// Full blob id from `POST /v1/blobs`, or `null`/absent to clear
    /// the avatar.
    #[serde(default)]
    pub blob_id: Option<String>,
}

// ── validation helpers ──────────────────────────────────────────────

fn validate_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(GatewayError::BadRequest(
            "agent name must not be empty".to_owned(),
        ));
    }
    if name.chars().count() > MAX_AGENT_PROFILE_NAME_CHARS {
        return Err(GatewayError::BadRequest(format!(
            "agent name exceeds {MAX_AGENT_PROFILE_NAME_CHARS} characters"
        )));
    }
    // The name lives on one line of `IDENTITY.md`, and reads derive it back by
    // parsing that line. A value that does not survive the round trip would
    // make the create response and the next `GET` disagree — the write keeps
    // the first line, the read strips surrounding markup — so it is rejected
    // rather than silently rewritten into something the caller did not ask
    // for. Checked by construction instead of by a character blocklist, so the
    // rule cannot drift from the two functions that implement it.
    let round_tripped = baybo_workspace::display_name(&baybo_workspace::with_display_name(
        IdentityKind::Identity.default_content(),
        name,
    ));
    if round_tripped.as_deref() != Some(name) {
        return Err(GatewayError::BadRequest(
            "agent name must be a single line of plain text (no newlines, and not markdown \
             emphasis on its own)"
                .to_owned(),
        ));
    }
    Ok(name.to_owned())
}

/// Reject a dangling or non-image avatar reference at write time; after
/// this the stored blob id is a soft reference (FKs are off).
async fn validate_avatar_blob(state: &AdminState, blob_id: &str) -> Result<()> {
    match state.blob_store.stat(blob_id).await {
        Ok(meta) if meta.mime_type.starts_with("image/") => Ok(()),
        Ok(meta) => Err(GatewayError::BadRequest(format!(
            "avatar blob must be an image, got {:?}",
            meta.mime_type
        ))),
        Err(StorageError::NotFound(_)) => Err(GatewayError::BadRequest(format!(
            "unknown blob id {:?}; upload via POST /v1/blobs first",
            redacted_blob_id(blob_id)
        ))),
        Err(e) => Err(GatewayError::Internal(format!("stat avatar blob: {e}"))),
    }
}

/// The part after the first `.` is the blob's read-token capability; never
/// echo it back in an error string (same convention as the blob store's
/// own `redacted_blob_id`) — error bodies get pasted into logs and issues.
fn redacted_blob_id(blob_id: &str) -> String {
    match blob_id.split_once('.') {
        Some((digest, _token)) => format!("{digest}.<redacted>"),
        None => blob_id.to_owned(),
    }
}

fn store_err(ctx: &str, e: StorageError) -> GatewayError {
    match e {
        StorageError::Conflict(m) => GatewayError::BadRequest(m),
        other => GatewayError::Internal(format!("{ctx}: {other}")),
    }
}

async fn load_agent(state: &AdminState, agent_id: &str) -> Result<AgentProfileRow> {
    // A path param that fails the id grammar can never name a row — and the
    // id joins onto the persona directory — so reject it here rather than
    // letting it reach the store as a miss.
    let id = parse_agent_id(agent_id)?;
    state
        .agent_profile_store
        .get(&id)
        .await
        .map_err(|e| GatewayError::Internal(format!("load agent profile: {e}")))?
        .ok_or_else(|| GatewayError::NotFound(format!("agent profile {agent_id}")))
}

/// Parse an untrusted agent id (path param, request body) into the
/// validated newtype, surfacing a failure as a 400 rather than a 404: the
/// value is malformed, not merely absent.
pub(crate) fn parse_agent_id(agent_id: &str) -> Result<AgentProfileId> {
    AgentProfileId::parse(agent_id).map_err(|e| GatewayError::BadRequest(e.to_string()))
}

const BUILTIN_FRAMEWORK_PINNED: &str =
    "the built-in agent always runs on baybo; its framework cannot be changed";

const BUILTIN_MODEL_FOLLOWS_DEFAULT: &str =
    "the built-in agent always follows `default-llm`; change that instead of pinning it";
const BUILTIN_UNDELETABLE: &str = "the built-in agent profile cannot be deleted";

// ── handlers ────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/agents",
    tag = "agents",
    responses(
        (status = 200, description = "Every agent profile, builtin first then by name", body = inline(ListResponse<AgentProfileDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_agents(
    State(state): State<AdminState>,
) -> Result<Json<ListResponse<AgentProfileDto>>> {
    let rows = state
        .agent_profile_store
        .list()
        .await
        .map_err(|e| GatewayError::Internal(format!("list agent profiles: {e}")))?;
    // One file read per agent — the price of the name living where the agent
    // can rewrite it. Concurrent, and the set is small (a roster, not a feed).
    let mut items: Vec<AgentProfileDto> =
        futures::future::join_all(rows.into_iter().map(|row| agent_dto(&state, row))).await;
    // The store can only order by id; the display order is by the derived
    // name, builtin first.
    items.sort_by(|a, b| {
        b.builtin
            .cmp(&a.builtin)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    post,
    path = "/agents",
    tag = "agents",
    request_body = CreateAgentProfileRequest,
    responses(
        (status = 200, description = "The created agent profile", body = AgentProfileDto),
        (status = 400, description = "Invalid/duplicate name, unknown LLM entry, or unknown/non-image avatar blob", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn create_agent(
    State(state): State<AdminState>,
    Json(req): Json<CreateAgentProfileRequest>,
) -> Result<Json<AgentProfileDto>> {
    let name = validate_name(&req.name)?;
    let llm = super::validate_llm_pin(&state, req.llm.as_deref())?;
    if let Some(blob_id) = req.avatar_blob_id.as_deref() {
        validate_avatar_blob(&state, blob_id).await?;
    }

    // µs precision so the returned DTO matches what a later GET reads
    // back from the µs-encoded columns.
    let now = Utc::now().trunc_subsecs(6);
    let row = AgentProfileRow {
        id: AgentProfileId::generate(),
        description: req.description,
        avatar_blob_id: req.avatar_blob_id,
        framework: req.framework.into(),
        llm,
        builtin: false,
        // `POST /v1/agents` opens a global chat persona. A project's team is
        // staffed through its own board (`/v1/projects/{pid}/agents`), which
        // is the only surface that mints a handle.
        team: None,
        hired_by: None,
        deleted_at: None,
        created_at: now,
        updated_at: now,
    };
    // Materialise the persona *before* publishing the row, so a filesystem
    // failure cannot leave a half-made agent in the roster. The row is what
    // makes an agent visible and what a retry would duplicate; the directory
    // is not — an orphaned one is inert (nothing sweeps persona folders, by
    // design) and the next attempt mints a fresh id anyway.
    let seed = req
        .soul
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| baybo_workspace::prompt::PERSONA_SOUL_TEMPLATE.to_owned());
    reject_oversized_identity(&seed)?;
    baybo_workspace::ensure_persona_layout(&state.workspace_paths, row.id.as_str(), &seed)
        .await
        .map_err(|e| GatewayError::Internal(format!("materialise agent persona: {e}")))?;
    set_display_name(&state, &row, &name).await?;
    state
        .agent_profile_store
        .create(&row)
        .await
        .map_err(|e| store_err("create agent profile", e))?;
    Ok(Json(AgentProfileDto::from_parts(row, name)))
}

#[utoipa::path(
    get,
    path = "/agents/{agent_id}",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    responses(
        (status = 200, description = "The agent profile", body = AgentProfileDto),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn get_agent(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentProfileDto>> {
    let row = load_agent(&state, &agent_id).await?;
    Ok(Json(agent_dto(&state, row).await))
}

#[utoipa::path(
    put,
    path = "/agents/{agent_id}",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    request_body = UpdateAgentProfileRequest,
    responses(
        (status = 204, description = "Agent profile replaced"),
        (status = 400, description = "Built-in profile is read-only, invalid/duplicate name, or unknown LLM entry", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn update_agent(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpdateAgentProfileRequest>,
) -> Result<axum::http::StatusCode> {
    let row = load_agent(&state, &agent_id).await?;
    // The builtin's description is ordinary editable text; only its framework
    // is pinned. Refuse an explicit change rather than silently ignoring one
    // — the store drops it either way, but a caller that asked deserves to
    // hear no.
    let requested: AgentFramework = req.framework.into();
    if row.builtin && requested != row.framework {
        return Err(GatewayError::BadRequest(
            BUILTIN_FRAMEWORK_PINNED.to_owned(),
        ));
    }
    let update = AgentProfileUpdate {
        description: req.description,
        framework: requested,
    };
    let matched = state
        .agent_profile_store
        .update(&row.id, &update)
        .await
        .map_err(|e| store_err("update agent profile", e))?;
    if !matched {
        // The row was loaded a moment ago, so a miss means it was deleted
        // concurrently.
        return Err(GatewayError::NotFound(format!("agent profile {agent_id}")));
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    put,
    path = "/agents/{agent_id}/name",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    request_body = SetAgentNameRequest,
    responses(
        (status = 204, description = "Name set"),
        (status = 400, description = "Malformed agent id or name, or a project agent, whose name its @handle was derived from and cannot outlive", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn set_agent_name(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetAgentNameRequest>,
) -> Result<axum::http::StatusCode> {
    // Targeted, and open to the builtin: a name is not a row field at all,
    // it is the `Name:` line of the agent's own `IDENTITY.md` — for the
    // builtin, the workspace's. The splice leaves every other line alone.
    let row = load_agent(&state, &agent_id).await?;
    let name = validate_name(&req.name)?;
    set_display_name(&state, &row, &name).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    put,
    path = "/agents/{agent_id}/model",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    request_body = SetAgentModelRequest,
    responses(
        (status = 204, description = "LLM pin set (or cleared)"),
        (status = 400, description = "Malformed agent id or unknown LLM entry", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn set_agent_model(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetAgentModelRequest>,
) -> Result<axum::http::StatusCode> {
    let row = load_agent(&state, &agent_id).await?;
    let llm = super::validate_llm_pin(&state, req.llm.as_deref())?;
    // The builtin follows `default-llm`; pinning it would put the same
    // decision in two places. Clearing is a no-op it can accept.
    if row.builtin && llm.is_some() {
        return Err(GatewayError::BadRequest(
            BUILTIN_MODEL_FOLLOWS_DEFAULT.to_owned(),
        ));
    }
    let matched = state
        .agent_profile_store
        .set_llm(&row.id, llm.as_ref())
        .await
        .map_err(|e| store_err("set agent llm", e))?;
    if !matched {
        return Err(GatewayError::NotFound(format!("agent profile {agent_id}")));
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Response body for the per-agent identity-file reads.
#[derive(Debug, Serialize, ToSchema)]
pub struct AgentIdentityFileDto {
    /// The markdown as it stands on disk.
    pub content: String,
    /// Hash of exactly the bytes in `content`. Pass it back on `PUT` to make
    /// the write conditional — see [`SetAgentIdentityFileRequest::version`].
    pub version: String,
    /// Absolute path this content came from — the agent's own
    /// `personas/<id>/<FILE>.md`. Surfaced so an operator knows what to edit
    /// and what to commit; it lives inside a git repo.
    pub path: String,
}

/// Request body for the per-agent identity-file writes.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetAgentIdentityFileRequest {
    pub content: String,
    /// The `version` from the `GET` this edit started from. When present the
    /// write is compare-and-set: a file that changed underneath returns 409
    /// and nothing is written.
    ///
    /// This is what makes it safe for a client to render *stale* content —
    /// which the web deliberately does, since it neither polls nor
    /// subscribes. Without it, an editor opened before the agent rewrote its
    /// own file would silently delete that rewrite on the next Save. Absent
    /// means unconditional last-write-wins, for a caller that genuinely
    /// means "set it to this" (a script, a restore).
    #[serde(default)]
    pub version: Option<String>,
}

/// Content hash used as the conditional-write token. Not a blob id — this is
/// a bare digest over the file's bytes, so an identical rewrite is a no-op
/// rather than a conflict.
fn content_version(content: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Read one of an agent's own identity files, seeding it on first read
/// exactly like the runtime's assembly does — so the editor never opens on a
/// phantom empty file.
async fn read_agent_identity_file(
    state: &AdminState,
    agent_id: &str,
    kind: IdentityKind,
) -> Result<Json<AgentIdentityFileDto>> {
    let row = load_agent(state, agent_id).await?;
    let path = row.id.identity_file(&state.workspace_paths, kind);
    let seed = match kind {
        IdentityKind::Soul => baybo_workspace::prompt::PERSONA_SOUL_TEMPLATE,
        other => other.default_content(),
    };
    let content = baybo_workspace::load_identity(baybo_workspace::IdentitySource::new(&path, seed))
        .await
        .map_err(|e| GatewayError::Internal(format!("read agent {kind:?} file: {e}")))?;
    Ok(Json(AgentIdentityFileDto {
        version: content_version(&content),
        content,
        path: baybo_workspace::absolutise(&path).display().to_string(),
    }))
}

/// The same ceiling the `Edit` tool enforces on an identity file
/// (`MAX_IDENTITY_BYTES`). Declared here rather than imported because the two
/// crates do not depend on each other; the number is the contract, and
/// `docs/modules/agent-profiles.md` states it once.
const MAX_IDENTITY_FILE_BYTES: usize = 1 << 20;

/// Refuse content the `Edit` tool would refuse.
///
/// Without this the admin API is the wide door: bodies up to the extractor's
/// limit were written straight through, and every one of those bytes then
/// rides in the system prompt of every turn that agent takes.
fn reject_oversized_identity(content: &str) -> Result<()> {
    if content.len() > MAX_IDENTITY_FILE_BYTES {
        return Err(GatewayError::BadRequest(format!(
            "identity file exceeds {} MiB",
            MAX_IDENTITY_FILE_BYTES >> 20
        )));
    }
    Ok(())
}

/// Replace one of an agent's own identity files.
///
/// Deliberately NOT behind the builtin lock: the built-in row is read-only,
/// but these are files, not row fields — and editing the built-in's own
/// persona is exactly what an operator expects its entry to offer. Racing
/// writes are refused rather than merged — see
/// [`SetAgentIdentityFileRequest::version`].
async fn write_agent_identity_file(
    state: &AdminState,
    agent_id: &str,
    kind: IdentityKind,
    req: &SetAgentIdentityFileRequest,
) -> Result<Json<AgentIdentityFileDto>> {
    let row = load_agent(state, agent_id).await?;
    reject_oversized_identity(&req.content)?;
    let path = row.id.identity_file(&state.workspace_paths, kind);
    // Held across the read, the comparison and the write: a compare-and-set
    // whose three steps another request can interleave with is not one.
    let _guard = identity_write_lock(&path).await;
    // What is actually on disk now, not what this request thinks was there. A
    // missing file reads as empty, so a fresh write after a delete conflicts
    // rather than silently resurrecting old content — and carries no name to
    // protect, which is how a deleted `IDENTITY.md` can be named again.
    let current = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    if let Some(expected) = req.version.as_deref()
        && content_version(&current) != expected
    {
        return Err(GatewayError::Conflict(format!(
            "{} changed since it was read (the agent may have rewritten it); \
             re-read it and reapply the edit",
            path.display()
        )));
    }
    replace_identity_file(state, &row, kind, &path, &current, &req.content).await?;
    // Return the new state, so an editor that stays open holds a fresh base
    // for its next conditional write instead of conflicting with itself.
    Ok(Json(AgentIdentityFileDto {
        version: content_version(&req.content),
        content: req.content.clone(),
        path: baybo_workspace::absolutise(&path).display().to_string(),
    }))
}

#[utoipa::path(
    get,
    path = "/agents/{agent_id}/soul",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    responses(
        (status = 200, description = "The agent's soul (personality, tone)", body = AgentIdentityFileDto),
        (status = 400, description = "Malformed agent id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn get_agent_soul(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentIdentityFileDto>> {
    read_agent_identity_file(&state, &agent_id, IdentityKind::Soul).await
}

#[utoipa::path(
    put,
    path = "/agents/{agent_id}/soul",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    request_body = SetAgentIdentityFileRequest,
    responses(
        (status = 200, description = "Soul replaced; carries the new version", body = AgentIdentityFileDto),
        (status = 400, description = "Malformed agent id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
        (status = 409, description = "The file changed since it was read", body = ErrorBody),
    )
)]
async fn set_agent_soul(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetAgentIdentityFileRequest>,
) -> Result<Json<AgentIdentityFileDto>> {
    write_agent_identity_file(&state, &agent_id, IdentityKind::Soul, &req).await
}

#[utoipa::path(
    get,
    path = "/agents/{agent_id}/identity",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    responses(
        (status = 200, description = "The agent's self-image (name, creature, vibe, emoji, avatar)", body = AgentIdentityFileDto),
        (status = 400, description = "Malformed agent id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn get_agent_identity(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentIdentityFileDto>> {
    read_agent_identity_file(&state, &agent_id, IdentityKind::Identity).await
}

#[utoipa::path(
    put,
    path = "/agents/{agent_id}/identity",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    request_body = SetAgentIdentityFileRequest,
    responses(
        (status = 200, description = "Self-image replaced; carries the new version", body = AgentIdentityFileDto),
        (status = 400, description = "Malformed agent id, or a body that renames a project agent — its @handle came from that name", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
        (status = 409, description = "The file changed since it was read", body = ErrorBody),
    )
)]
async fn set_agent_identity(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetAgentIdentityFileRequest>,
) -> Result<Json<AgentIdentityFileDto>> {
    write_agent_identity_file(&state, &agent_id, IdentityKind::Identity, &req).await
}

/// Stage through a sibling temp file and rename, so a concurrent reader (the
/// runtime assembling a system prompt) sees either the old file or the whole
/// new one — never a partial write.
///
/// The temp name is unique per call. A fixed one is shared state between
/// concurrent writers of the same file: one overwrites the other's staged
/// bytes before it renames, so the first installs content it never wrote and
/// the second fails on a temp file that is already gone.
async fn write_file_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("md.{}.{nonce}.tmp", std::process::id()));
    tokio::fs::write(&tmp, content).await?;
    let renamed = tokio::fs::rename(&tmp, path).await;
    if renamed.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    renamed
}

#[utoipa::path(
    put,
    path = "/agents/{agent_id}/avatar",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    request_body = SetAgentAvatarRequest,
    responses(
        (status = 204, description = "Avatar set (or cleared)"),
        (status = 400, description = "Unknown blob id or non-image mime", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn set_agent_avatar(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetAgentAvatarRequest>,
) -> Result<axum::http::StatusCode> {
    let row = load_agent(&state, &agent_id).await?;
    if let Some(blob_id) = req.blob_id.as_deref() {
        validate_avatar_blob(&state, blob_id).await?;
    }
    let matched = state
        .agent_profile_store
        .set_avatar(&row.id, req.blob_id.as_deref())
        .await
        .map_err(|e| GatewayError::Internal(format!("set agent avatar: {e}")))?;
    if !matched {
        return Err(GatewayError::NotFound(format!("agent profile {agent_id}")));
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/agents/{agent_id}",
    tag = "agents",
    params(("agent_id" = String, Path, description = "Agent profile id")),
    responses(
        (status = 204, description = "Agent profile deleted"),
        (status = 400, description = "Built-in profile cannot be deleted", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn delete_agent(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
) -> Result<axum::http::StatusCode> {
    let row = load_agent(&state, &agent_id).await?;
    if row.builtin {
        return Err(GatewayError::BadRequest(BUILTIN_UNDELETABLE.to_owned()));
    }
    let matched = state
        .agent_profile_store
        .delete(&row.id)
        .await
        .map_err(|e| GatewayError::Internal(format!("delete agent profile: {e}")))?;
    if !matched {
        return Err(GatewayError::NotFound(format!("agent profile {agent_id}")));
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation_trims_and_bounds() {
        assert_eq!(validate_name("  Helper  ").unwrap(), "Helper");
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"x".repeat(MAX_AGENT_PROFILE_NAME_CHARS)).is_ok());
        assert!(validate_name(&"x".repeat(MAX_AGENT_PROFILE_NAME_CHARS + 1)).is_err());
    }

    #[test]
    fn framework_dto_round_trips() {
        for f in AgentFramework::ALL.iter().copied() {
            let dto: AgentFrameworkDto = f.into();
            let back: AgentFramework = dto.into();
            assert_eq!(back, f);
            assert_eq!(
                serde_json::to_string(&dto).unwrap(),
                format!("\"{}\"", f.as_str()),
            );
        }
    }
}
