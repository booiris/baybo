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

impl From<AgentProfileRow> for AgentProfileDto {
    fn from(r: AgentProfileRow) -> Self {
        Self {
            id: r.id.into_inner(),
            name: r.name,
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
    pub name: String,
    pub description: String,
    pub framework: AgentFrameworkDto,
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
    let items = state
        .agent_profile_store
        .list()
        .await
        .map_err(|e| GatewayError::Internal(format!("list agent profiles: {e}")))?
        .into_iter()
        .map(AgentProfileDto::from)
        .collect();
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
        name,
        description: req.description,
        avatar_blob_id: req.avatar_blob_id,
        // Never written again: the column exists only to seed a persona
        // that predates the soul file (see `persona_soul_seed`).
        system_prompt: None,
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
        .unwrap_or_else(|| baybo_store::agent_profile::persona_soul_seed(&row));
    baybo_workspace::ensure_persona_layout(&state.workspace_paths, row.id.as_str(), &seed)
        .await
        .map_err(|e| GatewayError::Internal(format!("materialise agent persona: {e}")))?;
    Ok(Json(AgentProfileDto::from(row)))
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
    Ok(Json(AgentProfileDto::from(row)))
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
        name: validate_name(&req.name)?,
        description: req.description,
        system_prompt: None,
        framework: req.framework.into(),
        llm: super::validate_llm_pin(&state, req.llm.as_deref())?,
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

/// Response body for the per-agent identity-file reads.
#[derive(Debug, Serialize, ToSchema)]
pub struct AgentIdentityFileDto {
    /// The markdown as it stands on disk.
    pub content: String,
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
        IdentityKind::Soul => baybo_store::agent_profile::persona_soul_seed(&row),
        other => other.default_content().to_owned(),
    };
    let content =
        baybo_workspace::load_identity(baybo_workspace::IdentitySource::new(&path, &seed))
            .await
            .map_err(|e| GatewayError::Internal(format!("read agent {kind:?} file: {e}")))?;
    Ok(Json(AgentIdentityFileDto {
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
    content: &str,
) -> Result<axum::http::StatusCode> {
    let row = load_agent(state, agent_id).await?;
    let path = row.id.identity_file(&state.workspace_paths, kind);
    write_file_atomic(&path, content)
        .await
        .map_err(|e| GatewayError::Internal(format!("write agent {kind:?} file: {e}")))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
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
        (status = 204, description = "Soul replaced"),
        (status = 400, description = "Malformed agent id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn set_agent_soul(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetAgentIdentityFileRequest>,
) -> Result<axum::http::StatusCode> {
    write_agent_identity_file(&state, &agent_id, IdentityKind::Soul, &req.content).await
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
        (status = 204, description = "Self-image replaced"),
        (status = 400, description = "Malformed agent id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such agent profile", body = ErrorBody),
    )
)]
async fn set_agent_identity(
    State(state): State<AdminState>,
    Path(agent_id): Path<String>,
    Json(req): Json<SetAgentIdentityFileRequest>,
) -> Result<axum::http::StatusCode> {
    write_agent_identity_file(&state, &agent_id, IdentityKind::Identity, &req.content).await
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
