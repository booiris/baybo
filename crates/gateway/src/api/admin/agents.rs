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
async fn read_display_name(state: &AdminState, row: &AgentProfileRow) -> String {
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
async fn set_display_name(state: &AdminState, row: &AgentProfileRow, name: &str) -> Result<()> {
    let path = row
        .id
        .identity_file(&state.workspace_paths, IdentityKind::Identity);
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
    write_file_atomic(&path, &updated)
        .await
        .map_err(|e| GatewayError::Internal(format!("write agent identity: {e}")))
}

/// [`AgentProfileDto`] for one row, with its name read from disk.
async fn agent_dto(state: &AdminState, row: AgentProfileRow) -> AgentProfileDto {
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

const BUILTIN_READ_ONLY: &str = "the built-in agent profile is read-only (avatar excepted)";
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
        created_at: now,
        updated_at: now,
    };
    state
        .agent_profile_store
        .create(&row)
        .await
        .map_err(|e| store_err("create agent profile", e))?;
    // Materialise the persona directory now, so the agent has a soul and a
    // skills folder before its first session — and so the operator can see
    // where to edit them.
    let seed = req
        .soul
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| baybo_workspace::prompt::PERSONA_SOUL_TEMPLATE.to_owned());
    baybo_workspace::ensure_persona_layout(&state.workspace_paths, row.id.as_str(), &seed)
        .await
        .map_err(|e| GatewayError::Internal(format!("materialise agent persona: {e}")))?;
    // The requested name is written into the agent's own `IDENTITY.md`,
    // which is where a name lives — the row has no column for one.
    set_display_name(&state, &row, &name).await?;
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
    if row.builtin {
        return Err(GatewayError::BadRequest(BUILTIN_READ_ONLY.to_owned()));
    }
    let update = AgentProfileUpdate {
        description: req.description,
        framework: req.framework.into(),
    };
    let matched = state
        .agent_profile_store
        .update(&row.id, &update)
        .await
        .map_err(|e| store_err("update agent profile", e))?;
    if !matched {
        // The row read as non-builtin above, so the guard can't have
        // filtered it — it was deleted concurrently.
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
        (status = 400, description = "Malformed agent id or name", body = ErrorBody),
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
    // Also open to the builtin: which model the built-in assistant runs on is
    // a deployment choice, not part of what makes its row "default
    // behaviour". Its framework stays locked behind the full-replace PUT.
    let row = load_agent(&state, &agent_id).await?;
    let llm = super::validate_llm_pin(&state, req.llm.as_deref())?;
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
    /// `personas/<id>/<FILE>.md`, or the workspace `profile/<FILE>.md` for
    /// the built-in. Surfaced so an operator knows what to edit and what to
    /// commit; both live inside a git repo.
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

/// Replace one of an agent's own identity files.
///
/// Deliberately NOT behind the builtin lock: the built-in row is read-only,
/// but these are files, not row fields — and editing the workspace
/// `profile/` pair is exactly what an operator expects the built-in's entry
/// to offer. Racing writes are last-write-wins; both files are
/// git-versioned, so a clobber is recoverable.
async fn write_agent_identity_file(
    state: &AdminState,
    agent_id: &str,
    kind: IdentityKind,
    req: &SetAgentIdentityFileRequest,
) -> Result<Json<AgentIdentityFileDto>> {
    let row = load_agent(state, agent_id).await?;
    let path = row.id.identity_file(&state.workspace_paths, kind);
    if let Some(expected) = req.version.as_deref() {
        // Read what is actually on disk now, not what this request thinks was
        // there. A missing file hashes as empty, so a fresh write after a
        // delete conflicts rather than silently resurrecting old content.
        let current = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        let actual = content_version(&current);
        if actual != expected {
            return Err(GatewayError::Conflict(format!(
                "{} changed since it was read (the agent may have rewritten it); \
                 re-read it and reapply the edit",
                path.display()
            )));
        }
    }
    write_file_atomic(&path, &req.content)
        .await
        .map_err(|e| GatewayError::Internal(format!("write agent {kind:?} file: {e}")))?;
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
        (status = 400, description = "Malformed agent id", body = ErrorBody),
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

/// Stage through a sibling `.tmp` and rename, so a concurrent reader (the
/// runtime assembling a system prompt) sees either the old file or the whole
/// new one — never a partial write.
async fn write_file_atomic(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("md.tmp");
    tokio::fs::write(&tmp, content).await?;
    tokio::fs::rename(&tmp, path).await
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
