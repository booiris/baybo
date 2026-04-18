//! Shared request / response DTOs for the v1 API.
//!
//! Kept intentionally thin — most routes return the internal types
//! directly (`Session`, `Job`, `MemoryEntry`, …) since they already
//! derive `serde::Serialize`. This module only holds request bodies
//! and a few response wrappers where a raw list needs a top-level
//! envelope for forward-compat.

use serde::{Deserialize, Serialize};

use aura_model::ChannelType;

/// Envelope for list endpoints. Lets us add `next_cursor`, `total`,
/// etc. later without breaking clients that parse `items`.
#[derive(Debug, Serialize)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
}

impl<T> ListResponse<T> {
    pub fn new(items: Vec<T>) -> Self {
        Self { items }
    }
}

/// `POST /v1/sessions` body.
#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default)]
    pub channel: Option<ChannelType>,
}

/// `POST /v1/sessions/:id/messages` body.
#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub text: String,
}

/// Response for `POST /v1/sessions/:id/messages`. The HTTP reply
/// returns immediately; streaming deltas arrive on the SSE channel.
#[derive(Debug, Serialize)]
pub struct SendMessageResponse {
    pub message_id: String,
}

/// `POST /v1/memory` body.
#[derive(Debug, Deserialize)]
pub struct StoreMemoryRequest {
    pub content: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub importance: Option<f32>,
}

/// `GET /v1/memory` query params.
#[derive(Debug, Deserialize, Default)]
pub struct MemoryListQuery {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `POST /v1/cron` body. Schedule format is the standard 5-field cron
/// string accepted by [`aura_cron`].
#[derive(Debug, Deserialize)]
pub struct CreateCronRequest {
    pub schedule: String,
    pub user_id: String,
    #[serde(default)]
    pub channel: Option<ChannelType>,
    pub text: String,
    #[serde(default)]
    pub origin_session_id: Option<String>,
}
