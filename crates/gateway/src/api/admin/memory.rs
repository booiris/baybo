//! `/v1/memory` endpoints.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};

use aura_model::{MemoryCategory, MemoryEntry};

use crate::api::dto::{ListResponse, MemoryListQuery, StoreMemoryRequest};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/memory", get(list_memory).post(store_memory))
        .route("/memory/{id}", delete(delete_memory))
}

async fn list_memory(
    State(state): State<AdminState>,
    Query(q): Query<MemoryListQuery>,
) -> Result<Json<ListResponse<MemoryEntry>>> {
    let items = if let Some(query) = q.q.as_deref() {
        state
            .memory_manager
            .search(q.user_id.as_deref(), query, q.limit.unwrap_or(50))
            .await
    } else {
        state.memory_manager.list(q.user_id.as_deref()).await
    }
    .map_err(|e| GatewayError::Memory(e.to_string()))?;
    Ok(Json(ListResponse::new(items)))
}

async fn store_memory(
    State(state): State<AdminState>,
    Json(req): Json<StoreMemoryRequest>,
) -> Result<Json<MemoryEntry>> {
    let entry = MemoryEntry::new(
        req.user_id.unwrap_or_else(|| "http".to_owned()),
        req.content,
        MemoryCategory::KeyFact,
        req.importance.unwrap_or(0.5),
    );
    state
        .memory_manager
        .store(entry.clone())
        .await
        .map_err(|e| GatewayError::Memory(e.to_string()))?;
    Ok(Json(entry))
}

async fn delete_memory(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    state
        .memory_manager
        .delete(&id)
        .await
        .map_err(|e| GatewayError::Memory(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
