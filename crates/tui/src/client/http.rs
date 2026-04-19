//! Gateway client for the TUI — channel surface only.
//!
//! The TUI talks exclusively to the gateway's channel listener over a
//! Unix domain socket. Admin routes (status, config, jobs, cron, memory,
//! traces, skills, tools, llm, channels) are deliberately out of reach:
//! the TUI doesn't need them, and surfacing them would require a second
//! transport (TCP + bearer token) that doubles the wire surface for no
//! present user-visible gain.
//!
//! `GatewayClient` is a thin, DTO-shaped facade over [`UdsTransport`].
//! Each method maps one-to-one onto a channel route. SSE endpoints
//! return an [`SseStream`] that owns both the body and its driver task;
//! dropping the stream tears the connection down.

use std::path::PathBuf;

use aura_model::{ChatMessage, Session};
use aura_tools::{ApprovalDecision, ApprovalRequest};

use super::dto::{
    ClientResult, CreateSessionRequest, ListResponse, ResolveApprovalRequest,
    ResolveApprovalResponse, SendMessageRequest, SendMessageResponse,
};
use super::sse::SseStream;
use super::uds::UdsTransport;

/// UDS-backed client for an `aura gateway` channel listener.
#[derive(Clone)]
pub struct GatewayClient {
    channel: UdsTransport,
}

impl GatewayClient {
    /// Construct a new client bound to the channel UDS socket.
    ///
    /// * `channel_socket` — path of the gateway's channel listener.
    /// * `psk` — effective TUI PSK (derived via
    ///   `aura_gateway_auth::effective_tui_psk`).
    pub fn new(channel_socket: PathBuf, psk: [u8; 32]) -> Self {
        Self {
            channel: UdsTransport::new(channel_socket, psk),
        }
    }

    // ---- health ----

    /// `GET /healthz` — unauthenticated reachability probe against the
    /// channel listener.
    pub async fn healthz(&self) -> ClientResult<()> {
        self.channel.healthz().await
    }

    // ---- sessions ----

    pub async fn list_sessions(&self) -> ClientResult<Vec<Session>> {
        let env: ListResponse<Session> = self.channel.get_json("/v1/sessions").await?;
        Ok(env.items)
    }

    pub async fn create_session(&self, req: CreateSessionRequest) -> ClientResult<Session> {
        self.channel.post_json("/v1/sessions", &req).await
    }

    pub async fn get_session(&self, id: &str) -> ClientResult<Session> {
        self.channel.get_json(&format!("/v1/sessions/{id}")).await
    }

    pub async fn delete_session(&self, id: &str) -> ClientResult<()> {
        self.channel.delete(&format!("/v1/sessions/{id}")).await
    }

    pub async fn list_messages(&self, session_id: &str) -> ClientResult<Vec<ChatMessage>> {
        let env: ListResponse<ChatMessage> = self
            .channel
            .get_json(&format!("/v1/sessions/{session_id}/messages"))
            .await?;
        Ok(env.items)
    }

    pub async fn post_message(
        &self,
        session_id: &str,
        text: String,
    ) -> ClientResult<SendMessageResponse> {
        let body = SendMessageRequest { text };
        self.channel
            .post_json(&format!("/v1/sessions/{session_id}/messages"), &body)
            .await
    }

    /// Subscribe to `GET /v1/sessions/:id/stream`. Returns a stream that
    /// yields `SseEvent`s until the server closes the connection.
    pub async fn subscribe_session(
        &self,
        session_id: &str,
    ) -> ClientResult<SseStream<super::dto::SseEvent>> {
        self.channel
            .subscribe(&format!("/v1/sessions/{session_id}/stream"))
            .await
    }

    // ---- approvals ----

    pub async fn list_approvals(&self) -> ClientResult<Vec<ApprovalRequest>> {
        let env: ListResponse<ApprovalRequest> = self.channel.get_json("/v1/approvals").await?;
        Ok(env.items)
    }

    pub async fn resolve_approval(
        &self,
        call_id: &str,
        decision: ApprovalDecision,
    ) -> ClientResult<ResolveApprovalResponse> {
        let body = ResolveApprovalRequest { decision };
        self.channel
            .post_json(&format!("/v1/approvals/{call_id}"), &body)
            .await
    }

    pub async fn subscribe_approvals(&self) -> ClientResult<SseStream<super::dto::ApprovalEvent>> {
        self.channel.subscribe("/v1/approvals/stream").await
    }
}
