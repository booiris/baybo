//! Request/response DTOs for the gateway client.
//!
//! The shared SSE/approval event shapes (`SseEvent`, `ApprovalEvent`)
//! live in [`crate::wire`] so server and client compile against a
//! single canonical definition; the remaining types in this module are
//! request bodies and response envelopes specific to channel-plugin
//! calls against the gateway.

use aura_model::ChannelType;
use aura_tools::ApprovalDecision;
use serde::{Deserialize, Serialize};

/// Error from a gateway client call.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("unauthorized — check the gateway token")]
    Unauthorized,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("gateway error ({status}): {message}")]
    Server { status: u16, message: String },
    #[error("decode: {0}")]
    Decode(String),
}

pub type ClientResult<T> = Result<T, ClientError>;

/// List envelope used by every `GET .../<resource>` route. Matches the
/// gateway's `ListResponse<T>`.
#[derive(Debug, Clone, Deserialize)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
}

/// `POST /v1/sessions` body.
#[derive(Debug, Clone, Serialize, Default)]
pub struct CreateSessionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelType>,
}

/// `POST /v1/sessions/:id/messages` body.
#[derive(Debug, Clone, Serialize)]
pub struct SendMessageRequest {
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendMessageResponse {
    pub message_id: String,
}

/// `POST /v1/memory` body.
#[derive(Debug, Clone, Serialize)]
pub struct StoreMemoryRequest {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<f32>,
}

/// `POST /v1/cron` body.
#[derive(Debug, Clone, Serialize)]
pub struct CreateCronRequest {
    pub schedule: String,
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelType>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_session_id: Option<String>,
}

/// `POST /v1/approvals/:call_id` body.
#[derive(Debug, Clone, Serialize)]
pub struct ResolveApprovalRequest {
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveApprovalResponse {
    pub call_id: String,
    pub decision: ApprovalDecision,
}

/// `PUT /v1/config` body.
#[derive(Debug, Clone, Serialize)]
pub struct SetConfigRequest {
    pub path: String,
    pub value: serde_json::Value,
}

/// `DELETE /v1/config` body.
#[derive(Debug, Clone, Serialize)]
pub struct UnsetConfigRequest {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MutateConfigResponse {
    pub path: String,
    pub written_to: String,
    pub requires_restart: bool,
}

/// Summary of an [`aura_job::Job`] for dashboard rendering. The gateway
/// returns the full `Job` serde shape; we only pick the columns the
/// dashboard surfaces so the client doesn't drag in `aura-job`.
#[derive(Debug, Clone, Deserialize)]
pub struct JobSummary {
    pub id: String,
    pub session_id: String,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Summary of a [`aura_model::MemoryEntry`] for dashboard rendering.
#[derive(Debug, Clone, Deserialize)]
pub struct MemorySummary {
    pub id: String,
    pub user_id: String,
    pub category: String,
    pub importance: f32,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatusResponse {
    pub version: String,
    pub bind_address: String,
    pub sessions: usize,
    pub jobs_in_flight: usize,
}
