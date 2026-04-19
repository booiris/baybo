//! `POST /v1/sessions/{id}/messages` and `GET /v1/sessions/{id}/stream`.

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
use chrono::Utc;
use futures::Stream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use aura_channels::{IncomingMessage, Message};
use aura_model::{ChannelType, ContentBlock, MessageMetadata, User};

use crate::api::dto::{SendMessageRequest, SendMessageResponse};
use crate::http_adapter::SseEvent;
use crate::server::ChannelState;
use crate::{GatewayError, Result};

pub fn routes() -> Router<ChannelState> {
    Router::new()
        .route("/sessions/{id}/messages", post(send_message))
        .route("/sessions/{id}/stream", get(stream_session))
}

async fn send_message(
    State(state): State<ChannelState>,
    Path(session_id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<impl IntoResponse> {
    let session = state
        .session_manager
        .get(&session_id)
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("session {session_id}")))?;

    let sender = User {
        id: session.user.id.clone(),
        name: session.user.name.clone(),
        channel: ChannelType::Http,
    };
    let message_id = uuid::Uuid::new_v4().to_string();
    let msg = IncomingMessage {
        message: Message {
            id: message_id.clone(),
            session_id: session.id.clone(),
            channel: ChannelType::Http,
            sender,
            content: vec![ContentBlock::Text(req.text)],
            timestamp: Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        },
    };

    state
        .adapter
        .submit(msg)
        .await
        .map_err(|e| GatewayError::Channels(e.to_string()))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(SendMessageResponse { message_id }),
    ))
}

async fn stream_session(
    State(state): State<ChannelState>,
    Path(session_id): Path<String>,
) -> Result<Sse<impl Stream<Item = std::result::Result<Event, Infallible>>>> {
    if state
        .session_manager
        .get(&session_id)
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?
        .is_none()
    {
        return Err(GatewayError::NotFound(format!("session {session_id}")));
    }

    let rx = state.adapter.subscribe(&session_id).await;
    let broadcast = BroadcastStream::new(rx);

    let stream = try_stream! {
        let mut broadcast = broadcast;
        while let Some(next) = broadcast.next().await {
            match next {
                Ok(event) => {
                    let (name, payload) = event_fields(&event);
                    match Event::default().event(name).json_data(payload) {
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
                        .data("client lagged; some events were dropped");
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

fn event_fields(event: &SseEvent) -> (&'static str, &SseEvent) {
    let name = match event {
        SseEvent::Delta { .. } => "delta",
        SseEvent::Response { .. } => "response",
        SseEvent::Notice { .. } => "notice",
    };
    (name, event)
}
