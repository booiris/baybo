//! `/v1/approvals` — list, resolve, and stream pending tool approvals.

use std::convert::Infallible;
use std::time::Duration;

use async_stream::try_stream;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use aura_channels::ApprovalEvent;
use aura_tools::{ApprovalDecision, ApprovalRequest};

use crate::api::dto::ListResponse;
use crate::server::ChannelState;
use crate::{GatewayError, Result};

pub fn routes() -> Router<ChannelState> {
    Router::new()
        .route("/approvals", get(list_approvals))
        .route("/approvals/stream", get(stream_approvals))
        .route("/approvals/{call_id}", post(resolve_approval))
}

#[derive(Debug, Deserialize)]
struct ResolveRequest {
    decision: ApprovalDecision,
}

#[derive(Debug, Serialize)]
struct ResolveResponse {
    call_id: String,
    decision: ApprovalDecision,
}

async fn list_approvals(
    State(state): State<ChannelState>,
) -> Result<Json<ListResponse<ApprovalRequest>>> {
    let items = state.adapter.approval_queue().list();
    Ok(Json(ListResponse::new(items)))
}

async fn resolve_approval(
    State(state): State<ChannelState>,
    Path(call_id): Path<String>,
    Json(req): Json<ResolveRequest>,
) -> Result<impl IntoResponse> {
    let resolved = state
        .adapter
        .approval_queue()
        .resolve_by_call_id(&call_id, req.decision);
    if !resolved {
        return Err(GatewayError::NotFound(format!("approval {call_id}")));
    }
    state
        .adapter
        .publish_approval_resolved(call_id.clone(), req.decision);
    Ok((
        StatusCode::OK,
        Json(ResolveResponse {
            call_id,
            decision: req.decision,
        }),
    ))
}

async fn stream_approvals(
    State(state): State<ChannelState>,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    let rx = state.adapter.subscribe_approvals();
    let broadcast = BroadcastStream::new(rx);

    let stream = try_stream! {
        let mut broadcast = broadcast;
        while let Some(next) = broadcast.next().await {
            match next {
                Ok(event) => {
                    let name = event_name(&event);
                    match Event::default().event(name).json_data(event) {
                        Ok(evt) => yield evt,
                        Err(e) => {
                            yield Event::default()
                                .event("error")
                                .data(format!("encode error: {e}"));
                            break;
                        }
                    }
                }
                Err(_lag) => {
                    yield Event::default()
                        .event("error")
                        .data("client lagged; some approval events were dropped");
                }
            }
        }
        yield Event::default().event("end").data("");
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

fn event_name(event: &ApprovalEvent) -> &'static str {
    match event {
        ApprovalEvent::Added { .. } => "added",
        ApprovalEvent::Resolved { .. } => "resolved",
    }
}
