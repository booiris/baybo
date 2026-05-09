//! `/v1/memory` endpoints.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use aura_model::{MemoryCategory, MemoryEntry as MemoryEntryModel};

use crate::api::dto::{ErrorBody, ListResponse, MemoryEntry, MemoryListQuery, StoreMemoryRequest};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_memory, store_memory))
        .routes(routes!(delete_memory))
}

#[utoipa::path(
    get,
    path = "/memory",
    tag = "memory",
    params(MemoryListQuery),
    responses(
        (status = 200, description = "Matching memory entries", body = inline(ListResponse<MemoryEntry>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Memory store error", body = ErrorBody),
    )
)]
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
    .map_err(|e| GatewayError::Memory(e.to_string()))?
    .into_iter()
    .map(MemoryEntry::from)
    .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    post,
    path = "/memory",
    tag = "memory",
    request_body = StoreMemoryRequest,
    responses(
        (status = 200, description = "Stored memory entry", body = MemoryEntry),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Memory store error", body = ErrorBody),
    )
)]
async fn store_memory(
    State(state): State<AdminState>,
    Json(req): Json<StoreMemoryRequest>,
) -> Result<Json<MemoryEntry>> {
    let entry = MemoryEntryModel::new(
        req.user_id.unwrap_or_else(|| "http".to_owned()),
        req.content,
        MemoryCategory::User,
        req.importance.unwrap_or(0.5),
    );
    state
        .memory_manager
        .store(entry.clone())
        .await
        .map_err(|e| GatewayError::Memory(e.to_string()))?;
    Ok(Json(MemoryEntry::from(entry)))
}

#[utoipa::path(
    delete,
    path = "/memory/{id}",
    tag = "memory",
    params(
        ("id" = String, Path, description = "Memory entry id"),
    ),
    responses(
        (status = 204, description = "Memory entry deleted"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Memory store error", body = ErrorBody),
    )
)]
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
