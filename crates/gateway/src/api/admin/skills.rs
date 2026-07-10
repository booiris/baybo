//! `/v1/skills` — list registered skills with their descriptions.

use axum::Json;
use axum::extract::{Query, State};
use baybo_model::BUILTIN_AGENT_PROFILE_ID;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, ListResponse};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(list_skills))
}

/// One registered skill: its invocation name and a short blurb of what it
/// does (the `SKILL.md` `description`).
#[derive(Debug, Serialize, ToSchema)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

/// Query string for `GET /v1/skills`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct SkillsQuery {
    /// Scope the listing to this agent profile's skill folder overlaid on
    /// the shared set. Omitted or the builtin id ⇒ shared set only.
    pub agent_id: Option<String>,
}

#[utoipa::path(
    get,
    path = "/skills",
    tag = "skills",
    params(SkillsQuery),
    responses(
        (status = 200, description = "Registered skills with descriptions", body = inline(ListResponse<SkillInfo>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_skills(
    State(state): State<AdminState>,
    Query(q): Query<SkillsQuery>,
) -> Result<Json<ListResponse<SkillInfo>>> {
    let scope = q
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != BUILTIN_AGENT_PROFILE_ID);
    let items = state
        .skill_registry
        .summaries_for_agent(scope)
        .into_iter()
        .map(|s| SkillInfo {
            name: s.name,
            description: s.description,
        })
        .collect();
    Ok(Json(ListResponse::new(items)))
}
