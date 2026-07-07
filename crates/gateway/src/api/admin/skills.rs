//! `/v1/skills` — list registered skills with their descriptions.

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;
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

#[utoipa::path(
    get,
    path = "/skills",
    tag = "skills",
    responses(
        (status = 200, description = "Registered skills with descriptions", body = inline(ListResponse<SkillInfo>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_skills(State(state): State<AdminState>) -> Result<Json<ListResponse<SkillInfo>>> {
    let items = state
        .skill_registry
        .all_summaries_sorted()
        .into_iter()
        .map(|s| SkillInfo {
            name: s.name,
            description: s.description,
        })
        .collect();
    Ok(Json(ListResponse::new(items)))
}
