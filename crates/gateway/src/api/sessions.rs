//! `/v1/sessions` CRUD.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, response::IntoResponse};

use aura_model::{ChannelType, Session, User};

use crate::api::dto::{CreateSessionRequest, ListResponse};
use crate::server::ApiState;
use crate::{GatewayError, Result};

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/messages", get(list_messages))
}

async fn list_sessions(State(state): State<ApiState>) -> Result<Json<ListResponse<Session>>> {
    let items = state
        .session_manager
        .list()
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?;
    Ok(Json(ListResponse::new(items)))
}

async fn create_session(
    State(state): State<ApiState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse> {
    let channel = req.channel.unwrap_or(ChannelType::Http);
    let user = User {
        id: req
            .user_id
            .unwrap_or_else(|| format!("http:{}", uuid::Uuid::new_v4())),
        name: req.user_name,
        channel,
    };
    let session = state
        .session_manager
        .create_session(user, channel)
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(session)))
}

async fn get_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<Session>> {
    let session = state
        .session_manager
        .get(&id)
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("session {id}")))?;
    Ok(Json(session))
}

async fn delete_session(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    state
        .session_manager
        .delete(&id)
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_messages(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<ListResponse<aura_model::ChatMessage>>> {
    let items = state
        .session_manager
        .history(&id)
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?;
    Ok(Json(ListResponse::new(items)))
}
