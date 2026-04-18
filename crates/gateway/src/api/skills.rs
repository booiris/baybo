//! `/v1/skills` — list registered skills.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;

use crate::Result;
use crate::api::dto::ListResponse;
use crate::server::ApiState;

pub fn routes() -> Router<ApiState> {
    Router::new().route("/skills", get(list_skills))
}

async fn list_skills(State(state): State<ApiState>) -> Result<Json<ListResponse<String>>> {
    let items = state.skill_registry.list();
    Ok(Json(ListResponse::new(items)))
}
