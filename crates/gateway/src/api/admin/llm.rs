//! `/v1/llm` — snapshot of configured LLM provider(s).

use axum::Json;
use axum::extract::State;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, LlmInfo};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(get_llm))
}

#[utoipa::path(
    get,
    path = "/llm",
    tag = "llm",
    responses(
        (status = 200, description = "Configured LLM provider", body = LlmInfo),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn get_llm(State(state): State<AdminState>) -> Result<Json<LlmInfo>> {
    let info = state.llm_client.model_info();
    Ok(Json(LlmInfo {
        model_id: info.id.clone(),
        provider: info.provider.clone(),
    }))
}
