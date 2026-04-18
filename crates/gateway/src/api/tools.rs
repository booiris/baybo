//! `/v1/tools` — list registered tools with their manifests.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;

use crate::Result;
use crate::api::dto::ListResponse;
use crate::server::ApiState;

pub fn routes() -> Router<ApiState> {
    Router::new().route("/tools", get(list_tools))
}

async fn list_tools(
    State(state): State<ApiState>,
) -> Result<Json<ListResponse<aura_tools::ToolDefinition>>> {
    let items = state.tool_registry.tool_definitions();
    Ok(Json(ListResponse::new(items)))
}
