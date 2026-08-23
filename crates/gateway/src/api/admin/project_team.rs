//! `/v1/projects/{project_id}/agents` — one board's team.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use baybo_model::AgentProfileId;
use baybo_project::NewTeamMember;
use baybo_store::agent_profile::AgentProfileRow;

use super::agents::{AgentFrameworkDto, read_display_name};
use super::projects::{parse_project_id, project_err};
use crate::api::dto::{ErrorBody, ListResponse};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_team, hire_agent))
        .routes(routes!(remove_agent))
}

/// Who brought an agent onto the board. Absent means the operator did.
#[derive(Debug, Serialize, ToSchema)]
pub struct HiredByDto {
    pub id: String,
    pub handle: String,
}

/// One member of a project's team.
#[derive(Debug, Serialize, ToSchema)]
pub struct TeamMemberDto {
    pub id: String,
    /// Immutable `@handle` on this board — what a comment mentions.
    pub handle: String,
    /// Display name from the agent's own `IDENTITY.md`. Fixed at hire, like
    /// the handle derived from it.
    pub name: String,
    /// One line saying what this agent is for.
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_blob_id: Option<String>,
    pub framework: AgentFrameworkDto,
    /// The `baybo.json` entry this teammate's runs go through; absent
    /// follows `default-llm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<String>,
    /// The model within that entry; absent is the entry's default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// How hard this teammate thinks; absent is the entry's own rung. The
    /// board is the only place this is set — a card's run has no header to
    /// pick one from, so what the profile says is what the run gets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// The coordinator, which every board has and none may remove.
    pub lead: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hired_by: Option<HiredByDto>,
    pub created_at_ms: i64,
}

/// Request body for `POST /v1/projects/{project_id}/agents`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct HireAgentRequest {
    /// Display name, and the only chance to choose one: the `@handle` is
    /// derived from it here, and neither can be changed afterwards — a board
    /// that called an agent one thing while everybody addressed it as another
    /// would be lying on every card.
    pub name: String,
    /// One line saying what this agent is for. Seeds its `SOUL.md` and
    /// becomes its roster description.
    pub role: String,
    /// The operator's form may pin a framework; `ProjectAgentCreate`
    /// deliberately exposes neither this nor `llm`.
    #[serde(default)]
    pub framework: Option<AgentFrameworkDto>,
    /// `baybo.json` LLM entry name; must match a configured entry.
    #[serde(default)]
    pub llm: Option<String>,
    /// The model within `llm`'s entry, or absent for that entry's default.
    /// Requires `llm`.
    #[serde(default)]
    pub model: Option<String>,
    /// Thinking rung, or absent for the entry's own level.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

async fn member_dto(state: &AdminState, row: AgentProfileRow) -> TeamMemberDto {
    let name = read_display_name(state, &row).await;
    let handle = row
        .team
        .as_ref()
        .map(|team| team.handle.as_str().to_owned())
        // Unreachable through `list_team`, which selects on the column.
        // Falling back rather than failing keeps one malformed row from
        // blanking the whole strip.
        .unwrap_or_default();
    let hired_by = match row.hired_by.as_ref() {
        Some(id) => state
            .agent_profile_store
            .get(id)
            .await
            .ok()
            .flatten()
            .and_then(|hirer| {
                hirer.team.map(|team| HiredByDto {
                    id: id.as_str().to_owned(),
                    handle: team.handle.as_str().to_owned(),
                })
            }),
        None => None,
    };
    TeamMemberDto {
        id: row.id.as_str().to_owned(),
        lead: handle == baybo_project::LEAD_HANDLE,
        handle,
        name,
        description: row.description,
        avatar_blob_id: row.avatar_blob_id,
        framework: row.framework.into(),
        llm: row.llm.entry.map(|l| l.to_string()),
        model: row.llm.model,
        reasoning_effort: row.llm.effort,
        hired_by,
        created_at_ms: row.created_at.timestamp_millis(),
    }
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/agents",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    responses(
        (status = 200, description = "This project's team, by handle", body = inline(ListResponse<TeamMemberDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
    )
)]
async fn list_team(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
) -> Result<Json<ListResponse<TeamMemberDto>>> {
    let id = parse_project_id(&project_id)?;
    let rows = state.project_manager.team(&id).await.map_err(project_err)?;
    // One file read per member — the price of a name living where the agent
    // can rewrite it. Concurrent, and a team is capped at sixteen.
    let items =
        futures::future::join_all(rows.into_iter().map(|row| member_dto(&state, row))).await;
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/agents",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    request_body = HireAgentRequest,
    responses(
        (status = 201, description = "The new teammate", body = TeamMemberDto),
        (status = 400, description = "Unusable name, missing role, full team, or unknown LLM entry", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
        (status = 409, description = "The project is archived", body = ErrorBody),
    )
)]
async fn hire_agent(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
    Json(req): Json<HireAgentRequest>,
) -> Result<(StatusCode, Json<TeamMemberDto>)> {
    let id = parse_project_id(&project_id)?;
    let llm = super::validate_llm_pin(
        &state,
        req.llm.as_deref(),
        req.model.as_deref(),
        req.reasoning_effort.as_deref(),
    )?;
    let row = state
        .project_manager
        .hire(
            &id,
            NewTeamMember {
                name: req.name,
                role: req.role,
                framework: req.framework.map(Into::into),
                llm,
            },
            // The operator is at the keyboard on this route. A hire made by
            // an agent goes through `ProjectAgentCreate`, which names it.
            None,
        )
        .await
        .map_err(project_err)?;
    Ok((StatusCode::CREATED, Json(member_dto(&state, row).await)))
}

#[utoipa::path(
    delete,
    path = "/projects/{project_id}/agents/{agent_id}",
    tag = "projects",
    params(
        ("project_id" = String, Path, description = "Project id"),
        ("agent_id" = String, Path, description = "Agent profile id"),
    ),
    responses(
        (status = 204, description = "The agent left the team; its past work still names it"),
        (status = 400, description = "Not on this team, the lead, or an agent with a run in flight", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
        (status = 409, description = "The project is archived", body = ErrorBody),
    )
)]
async fn remove_agent(
    State(state): State<AdminState>,
    Path((project_id, agent_id)): Path<(String, String)>,
) -> Result<StatusCode> {
    let id = parse_project_id(&project_id)?;
    let agent =
        AgentProfileId::parse(agent_id).map_err(|e| GatewayError::BadRequest(e.to_string()))?;
    state
        .project_manager
        .remove_from_team(&id, &agent)
        .await
        .map_err(project_err)?;
    Ok(StatusCode::NO_CONTENT)
}
