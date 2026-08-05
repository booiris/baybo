//! `/v1/projects/{project_id}/agents` — one board's team.
//!
//! Separate from `/v1/agents` because the two rosters are disjoint by
//! construction: that surface lists global chat personas, this one lists
//! the agents that belong to a project. A teammate is addressed by
//! `@handle` on its board and is never offered as a chat persona; a global
//! persona has no handle here and cannot be assigned an issue.

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
        .routes(routes!(list_lead_conversations, open_lead_conversation))
}

// ── DTOs ────────────────────────────────────────────────────────────────

/// Who brought an agent onto the board. Absent means the operator did.
///
/// The handle rides along because that is what gets rendered — resolved
/// here rather than by the client, since a hire made by an agent that has
/// since been removed is not in the team list the client holds.
#[derive(Debug, Serialize, ToSchema)]
pub struct HiredByDto {
    pub id: String,
    pub handle: String,
}

/// One member of a project's team.
///
/// Deliberately carries no "is working" flag: the board already subscribes
/// to run frames, and a second copy of that state on a roster read is a
/// copy that can disagree with the shimmer on the card.
#[derive(Debug, Serialize, ToSchema)]
pub struct TeamMemberDto {
    pub id: String,
    /// Immutable `@handle` on this board — what a comment mentions.
    pub handle: String,
    /// Display name from the agent's own `IDENTITY.md`.
    pub name: String,
    /// One line saying what this agent is for.
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_blob_id: Option<String>,
    pub framework: AgentFrameworkDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<String>,
    /// The coordinator, which every board has and none may remove.
    pub lead: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hired_by: Option<HiredByDto>,
    pub created_at_ms: i64,
}

/// Request body for `POST /v1/projects/{project_id}/agents`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct HireAgentRequest {
    /// Display name. The `@handle` is derived from it and then immutable —
    /// renaming the agent later never moves its handle.
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
}

/// Build one wire row, reading the display name off the agent's own file
/// and resolving whoever hired it.
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
        llm: row.llm.map(|l| l.to_string()),
        hired_by,
        created_at_ms: row.created_at.timestamp_millis(),
    }
}

// ── Handlers ────────────────────────────────────────────────────────────

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
    let llm = super::validate_llm_pin(&state, req.llm.as_deref())?;
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

// ── The lead's planning conversations ───────────────────────────────────

/// One conversation with a board's lead.
#[derive(Debug, Serialize, ToSchema)]
pub struct LeadConversationDto {
    /// The session id. Everything else — history, live traffic, sending —
    /// goes through the ordinary chat transport, which scopes by channel
    /// and is happy to carry a board's session; only the chat *list*
    /// filters these out, which is the point.
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub last_active_ms: i64,
    pub created_at_ms: i64,
}

#[utoipa::path(
    get,
    path = "/projects/{project_id}/lead/conversations",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    responses(
        (status = 200, description = "This board's conversations with its lead, most recently active first", body = inline(ListResponse<LeadConversationDto>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project", body = ErrorBody),
    )
)]
async fn list_lead_conversations(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
) -> Result<Json<ListResponse<LeadConversationDto>>> {
    let id = parse_project_id(&project_id)?;
    state
        .project_manager
        .get_project(&id)
        .await
        .map_err(project_err)?;
    let items = state
        .session_manager
        .list_project_conversations(id.as_str())
        .await
        .map_err(|e| GatewayError::Internal(format!("list lead conversations: {e}")))?
        .into_iter()
        .map(|session| LeadConversationDto {
            session_id: session.id.as_str().to_owned(),
            title: session.title,
            last_active_ms: session.last_active.timestamp_millis(),
            created_at_ms: session.created_at.timestamp_millis(),
        })
        .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    post,
    path = "/projects/{project_id}/lead/conversations",
    tag = "projects",
    params(("project_id" = String, Path, description = "Project id")),
    responses(
        (status = 201, description = "A fresh conversation with the lead", body = LeadConversationDto),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Unknown project, or it has no lead", body = ErrorBody),
        (status = 409, description = "The project is archived", body = ErrorBody),
    )
)]
async fn open_lead_conversation(
    State(state): State<AdminState>,
    Path(project_id): Path<String>,
) -> Result<(StatusCode, Json<LeadConversationDto>)> {
    let id = parse_project_id(&project_id)?;
    let project = state
        .project_manager
        .get_project(&id)
        .await
        .map_err(project_err)?;
    if project.archived_at.is_some() {
        return Err(GatewayError::Conflict(format!(
            "project {id} is archived and takes no new work"
        )));
    }
    // Bound to the lead, not to the built-in: the conversation has to run
    // on the coordinator's soul and memory partition, and its tool calls
    // are recorded on the board as the lead's.
    let lead = state
        .project_manager
        .team(&id)
        .await
        .map_err(project_err)?
        .into_iter()
        .find(|row| {
            row.team
                .as_ref()
                .is_some_and(|team| team.handle.as_str() == baybo_project::LEAD_HANDLE)
        })
        .ok_or_else(|| GatewayError::NotFound(format!("project {id} has no lead")))?;

    let channel = baybo_model::ChannelType::owner();
    let session = state
        .session_manager
        .create_bound_session_with_trigger(
            baybo_model::User {
                id: crate::auth::OWNER_USER_ID.to_owned(),
                name: None,
                channel: channel.clone(),
            },
            channel,
            baybo_model::TriggerSource::Project {
                project_id: id.clone(),
            },
            Some(baybo_model::AgentBinding {
                agent_id: lead.id.clone(),
                framework: lead.framework,
            }),
        )
        .await
        .map_err(|e| GatewayError::Internal(format!("open a lead conversation: {e}")))?;
    Ok((
        StatusCode::CREATED,
        Json(LeadConversationDto {
            session_id: session.id.as_str().to_owned(),
            title: session.title,
            last_active_ms: session.last_active.timestamp_millis(),
            created_at_ms: session.created_at.timestamp_millis(),
        }),
    ))
}
