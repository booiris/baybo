//! `/v1/goals` + per-session goal read/pause/clear endpoints.
//!
//! Pause/clear write the store via [`baybo_goal::GoalService`]; the running actor
//! reads live goal status at every turn boundary, so the store write is honoured
//! on the next boundary without an actor round-trip. Resume is deliberately not
//! an operator control: it must re-arm the continuation loop, which only the
//! in-session `/goal resume` path does — a bare store write cannot.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use baybo_goal::{GoalService, PauseOutcome};
use baybo_model::SessionId;

use crate::api::dto::{ErrorBody, GoalItem, GoalsResponse, SessionGoalResponse};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_goals))
        .routes(routes!(get_session_goal))
        .routes(routes!(pause_session_goal))
        .routes(routes!(clear_session_goal))
}

fn service(state: &AdminState) -> GoalService {
    GoalService::new(Arc::clone(&state.goal_store))
}

/// Cross-session view of every session's current goal — the dashboard goals
/// column.
#[utoipa::path(
    get,
    path = "/goals",
    tag = "goals",
    responses(
        (status = 200, description = "Every session's current goal", body = GoalsResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Goal store error", body = ErrorBody),
    )
)]
async fn list_goals(State(state): State<AdminState>) -> Result<Json<GoalsResponse>> {
    let goals = service(&state)
        .list_all()
        .await
        .map_err(|e| GatewayError::Internal(e.to_string()))?
        .iter()
        .map(|(sid, goal)| GoalItem::from_goal(sid, goal))
        .collect();
    Ok(Json(GoalsResponse { goals }))
}

/// One session's current goal — the chat goal banner. `goal` is `null` when
/// none is set.
#[utoipa::path(
    get,
    path = "/chat/sessions/{session_id}/goal",
    tag = "goals",
    params(("session_id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "The session's current goal (or null)", body = SessionGoalResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Goal store error", body = ErrorBody),
    )
)]
async fn get_session_goal(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> Result<Json<SessionGoalResponse>> {
    let sid = SessionId::from(session_id);
    let goal = service(&state)
        .current(&sid)
        .await
        .map_err(|e| GatewayError::Internal(e.to_string()))?
        .map(|g| GoalItem::from_goal(&sid, &g));
    Ok(Json(SessionGoalResponse { goal }))
}

/// Operator pause: flip an `Active` goal to `Paused` so the continuation loop
/// stops at the next turn boundary. A no-op (returns the goal unchanged) when
/// no goal is set or it isn't currently active.
#[utoipa::path(
    post,
    path = "/chat/sessions/{session_id}/goal/pause",
    tag = "goals",
    params(("session_id" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "The goal after pausing (or null)", body = SessionGoalResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Goal store error", body = ErrorBody),
    )
)]
async fn pause_session_goal(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> Result<Json<SessionGoalResponse>> {
    let sid = SessionId::from(session_id);
    let goal = match service(&state)
        .pause(&sid)
        .await
        .map_err(|e| GatewayError::Internal(e.to_string()))?
    {
        PauseOutcome::Paused(g) | PauseOutcome::NotActive(g) => Some(GoalItem::from_goal(&sid, &g)),
        PauseOutcome::NoGoal => None,
    };
    Ok(Json(SessionGoalResponse { goal }))
}

/// Operator clear: the one explicit per-row goal delete. The continuation loop
/// stops at the next boundary (no goal row to read).
#[utoipa::path(
    delete,
    path = "/chat/sessions/{session_id}/goal",
    tag = "goals",
    params(("session_id" = String, Path, description = "Session id")),
    responses(
        (status = 204, description = "Goal cleared (or none was set)"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Goal store error", body = ErrorBody),
    )
)]
async fn clear_session_goal(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> Result<axum::http::StatusCode> {
    let sid = SessionId::from(session_id);
    service(&state)
        .clear(&sid)
        .await
        .map_err(|e| GatewayError::Internal(e.to_string()))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
