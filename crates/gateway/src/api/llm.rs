//! `/v1/llm` — snapshot of configured LLM provider(s).

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde::Serialize;

use crate::Result;
use crate::server::ApiState;

pub fn routes() -> Router<ApiState> {
    Router::new().route("/llm", get(get_llm))
}

#[derive(Debug, Serialize)]
pub struct LlmInfo {
    pub model_id: String,
    pub provider: String,
}

async fn get_llm(State(state): State<ApiState>) -> Result<Json<LlmInfo>> {
    let info = state.llm_client.model_info();
    Ok(Json(LlmInfo {
        model_id: info.id.clone(),
        provider: info.provider.clone(),
    }))
}
