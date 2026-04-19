//! `/v1/tools` — list registered tools with their manifests.

use axum::Json;
use axum::extract::State;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, ListResponse, ToolDefinition};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(list_tools))
}

#[utoipa::path(
    get,
    path = "/tools",
    tag = "tools",
    responses(
        (status = 200, description = "Registered tool manifests", body = inline(ListResponse<ToolDefinition>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_tools(State(state): State<AdminState>) -> Result<Json<ListResponse<ToolDefinition>>> {
    let items = state
        .tool_registry
        .tool_definitions()
        .into_iter()
        .map(ToolDefinition::from)
        .collect();
    Ok(Json(ListResponse::new(items)))
}
