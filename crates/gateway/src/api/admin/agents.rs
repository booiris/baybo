//! `/v1/agents` — CRUD for user-managed agent profiles (chat personas).
//!
//! Management-only in v1: no runtime consumer reads profiles yet. The
//! seeded built-in profile is read-only except its avatar and cannot be
//! deleted; content updates are a full replace, the avatar rides its own
//! targeted endpoint. See `docs/modules/agent-profiles.md`.

use axum::Json;
use axum::extract::{Path, State};
use baybo_model::{
    AgentFramework, AgentProfileId, LlmEntryName, MAX_AGENT_PROFILE_NAME_CHARS, ReasoningEffort,
};
use baybo_store::StorageError;
use baybo_store::agent_profile::{AgentProfileRow, AgentProfileUpdate};
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

/// One agent profile. Absent `system_prompt` = workspace Soul; absent
/// `llm` = follow `default-llm`. Skills are not part of the profile — they
/// are read live from the skill registry (`GET /v1/skills`).
#[derive(Debug, Serialize, ToSchema)]
pub struct AgentProfileDto {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_blob_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub framework: AgentFrameworkDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<String>,
    /// LLM entries a bound session may switch to. Empty = unrestricted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_models: Vec<String>,
    /// Per-request reasoning effort for providers that support it.
    /// Absent = follow the LLM entry's own configured value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
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
            system_prompt: r.system_prompt,
            framework: r.framework.into(),
            llm: r.llm.map(|l| l.to_string()),
            allowed_models: r.allowed_models.iter().map(|n| n.to_string()).collect(),
            reasoning_effort: r.reasoning_effort.map(|e| e.as_str().to_owned()),
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
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// `baybo.json` LLM entry name; must match a configured entry — see
    /// `GET /v1/llm/models`.
    #[serde(default)]
    pub llm: Option<String>,
    /// LLM entries a bound session may switch to; each must match a
    /// configured entry. Empty = unrestricted. When `llm` is also set it
    /// must be a member of this set.
    #[serde(default)]
    pub allowed_models: Vec<String>,
    /// Per-request reasoning effort for providers that support it; see
    /// [`baybo_model::ReasoningEffort::ALL`] for legal values. Empty/absent
    /// = follow the LLM entry's own configured value.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
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
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub llm: Option<String>,
    /// LLM entries a bound session may switch to; each must match a
    /// configured entry. Empty = unrestricted. When `llm` is also set it
    /// must be a member of this set.
    #[serde(default)]
    pub allowed_models: Vec<String>,
    /// Per-request reasoning effort for providers that support it; see
    /// [`baybo_model::ReasoningEffort::ALL`] for legal values. Empty/absent
    /// = follow the LLM entry's own configured value.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
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

/// Validate + normalize the allowed-models set: every member must be a live
/// pool entry; duplicates collapse (order preserved); when a pin is also
/// set, it must be a member.
fn validate_allowed_models(
    state: &AdminState,
    raw: &[String],
    pin: Option<&LlmEntryName>,
) -> Result<Vec<LlmEntryName>> {
    let mut out: Vec<LlmEntryName> = Vec::with_capacity(raw.len());
    for name in raw {
        let entry = super::validate_llm_pin(state, Some(name))?.ok_or_else(|| {
            GatewayError::BadRequest("allowed_models entries must not be empty".to_owned())
        })?;
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    if let (Some(pin), false) = (pin, out.is_empty())
        && !out.contains(pin)
    {
        return Err(GatewayError::BadRequest(format!(
            "llm pin {:?} is not in allowed_models",
            pin.as_str()
        )));
    }
    Ok(out)
}

fn validate_reasoning_effort(raw: Option<&str>) -> Result<Option<ReasoningEffort>> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => match ReasoningEffort::parse(s) {
            Some(e) => Ok(Some(e)),
            None => Err(GatewayError::BadRequest(format!(
                "unknown reasoning_effort {s:?}; expected one of {}",
                ReasoningEffort::ALL
                    .iter()
                    .map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        },
    }
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
    let id = AgentProfileId::from(agent_id);
    state
        .agent_profile_store
        .get(&id)
        .await
        .map_err(|e| GatewayError::Internal(format!("load agent profile: {e}")))?
        .ok_or_else(|| GatewayError::NotFound(format!("agent profile {agent_id}")))
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
    let allowed_models = validate_allowed_models(&state, &req.allowed_models, llm.as_ref())?;
    let reasoning_effort = validate_reasoning_effort(req.reasoning_effort.as_deref())?;
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
        system_prompt: req.system_prompt,
        framework: req.framework.into(),
        llm,
        allowed_models,
        reasoning_effort,
        builtin: false,
        created_at: now,
        updated_at: now,
    };
    state
        .agent_profile_store
        .create(&row)
        .await
        .map_err(|e| store_err("create agent profile", e))?;
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
    let name = validate_name(&req.name)?;
    let llm = super::validate_llm_pin(&state, req.llm.as_deref())?;
    let allowed_models = validate_allowed_models(&state, &req.allowed_models, llm.as_ref())?;
    let reasoning_effort = validate_reasoning_effort(req.reasoning_effort.as_deref())?;
    let update = AgentProfileUpdate {
        name,
        description: req.description,
        system_prompt: req.system_prompt,
        framework: req.framework.into(),
        llm,
        allowed_models,
        reasoning_effort,
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
