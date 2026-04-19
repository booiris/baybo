//! `/v1/skills` — list registered skills.

use axum::Json;
use axum::extract::State;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, ListResponse};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(list_skills))
}

#[utoipa::path(
    get,
    path = "/skills",
    tag = "skills",
    responses(
        (status = 200, description = "Registered skill names", body = inline(ListResponse<String>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_skills(State(state): State<AdminState>) -> Result<Json<ListResponse<String>>> {
    let items = state.skill_registry.list();
    Ok(Json(ListResponse::new(items)))
}
