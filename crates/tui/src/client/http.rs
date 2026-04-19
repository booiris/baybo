//! `reqwest`-based HTTP client for the `aura-gateway` v1 API.
//!
//! Each method maps one-to-one to a gateway route. Responses are
//! deserialized into the DTOs in [`super::dto`] or — for routes whose
//! body is an entity already serde-derived in `aura-model` or other
//! workspace crates — into those types directly.
//!
//! The client holds no session or cursor state; all call sites pass
//! explicit ids. Cloning is cheap (`reqwest::Client` is already
//! `Arc`-backed internally).

use std::time::Duration;

use aura_model::{ChatMessage, Session};
use aura_tools::{ApprovalDecision, ApprovalRequest};
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;

use super::dto::{
    ClientError, ClientResult, CreateCronRequest, CreateSessionRequest, JobSummary, ListResponse,
    MemorySummary, MutateConfigResponse, ResolveApprovalRequest, ResolveApprovalResponse,
    SendMessageRequest, SendMessageResponse, SetConfigRequest, StatusResponse, StoreMemoryRequest,
    UnsetConfigRequest,
};
use super::sse::SseStream;

/// Default timeout for non-streaming HTTP calls. SSE endpoints opt out
/// (they're long-lived by construction).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP client for an `aura gateway`.
#[derive(Clone)]
pub struct GatewayClient {
    base_url: String,
    token: String,
    inner: Client,
}

impl GatewayClient {
    /// Construct a new client. `base_url` is the gateway's origin
    /// (e.g. `http://127.0.0.1:7777`); it is normalized to drop any
    /// trailing slash so the per-method path concatenation is
    /// unambiguous.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> ClientResult<Self> {
        let base_url = base_url.into();
        let base_url = base_url.trim_end_matches('/').to_string();
        let token = token.into();
        let inner = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        Ok(Self {
            base_url,
            token,
            inner,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get_json<R: DeserializeOwned>(&self, path: &str) -> ClientResult<R> {
        let resp = self
            .inner
            .get(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        parse_json(resp).await
    }

    async fn post_json<B: serde::Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ClientResult<R> {
        let resp = self
            .inner
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        parse_json(resp).await
    }

    async fn put_json<B: serde::Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ClientResult<R> {
        let resp = self
            .inner
            .put(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        parse_json(resp).await
    }

    async fn delete(&self, path: &str) -> ClientResult<()> {
        let resp = self
            .inner
            .delete(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        parse_empty(resp).await
    }

    async fn delete_with_body<B: serde::Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ClientResult<R> {
        let resp = self
            .inner
            .delete(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        parse_json(resp).await
    }

    // ---- health / status / config / llm ----

    /// `GET /healthz` — unauthenticated. Returns `()` on any 2xx.
    pub async fn healthz(&self) -> ClientResult<()> {
        let resp = self
            .inner
            .get(self.url("/healthz"))
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        parse_empty(resp).await
    }

    pub async fn status(&self) -> ClientResult<StatusResponse> {
        self.get_json("/v1/status").await
    }

    pub async fn get_config(&self) -> ClientResult<serde_json::Value> {
        self.get_json("/v1/config").await
    }

    pub async fn set_config(
        &self,
        path: &str,
        value: serde_json::Value,
    ) -> ClientResult<MutateConfigResponse> {
        let body = SetConfigRequest {
            path: path.to_string(),
            value,
        };
        self.put_json("/v1/config", &body).await
    }

    pub async fn unset_config(&self, path: &str) -> ClientResult<MutateConfigResponse> {
        let body = UnsetConfigRequest {
            path: path.to_string(),
        };
        self.delete_with_body("/v1/config", &body).await
    }

    pub async fn get_llm(&self) -> ClientResult<serde_json::Value> {
        self.get_json("/v1/llm").await
    }

    // ---- sessions ----

    pub async fn list_sessions(&self) -> ClientResult<Vec<Session>> {
        let env: ListResponse<Session> = self.get_json("/v1/sessions").await?;
        Ok(env.items)
    }

    pub async fn create_session(&self, req: CreateSessionRequest) -> ClientResult<Session> {
        self.post_json("/v1/sessions", &req).await
    }

    pub async fn get_session(&self, id: &str) -> ClientResult<Session> {
        self.get_json(&format!("/v1/sessions/{id}")).await
    }

    pub async fn delete_session(&self, id: &str) -> ClientResult<()> {
        self.delete(&format!("/v1/sessions/{id}")).await
    }

    pub async fn list_messages(&self, session_id: &str) -> ClientResult<Vec<ChatMessage>> {
        let env: ListResponse<ChatMessage> = self
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
        self.post_json(&format!("/v1/sessions/{session_id}/messages"), &body)
            .await
    }

    /// Subscribe to `GET /v1/sessions/:id/stream`. Returns a stream that
    /// yields `SseEvent`s until the server closes the connection.
    pub async fn subscribe_session(
        &self,
        session_id: &str,
    ) -> ClientResult<SseStream<super::dto::SseEvent>> {
        self.subscribe(&format!("/v1/sessions/{session_id}/stream"))
            .await
    }

    // ---- jobs ----

    pub async fn list_jobs(&self) -> ClientResult<Vec<JobSummary>> {
        let env: ListResponse<JobSummary> = self.get_json("/v1/jobs").await?;
        Ok(env.items)
    }

    pub async fn get_job(&self, id: &str) -> ClientResult<serde_json::Value> {
        self.get_json(&format!("/v1/jobs/{id}")).await
    }

    pub async fn cancel_job(&self, id: &str) -> ClientResult<()> {
        let resp = self
            .inner
            .post(self.url(&format!("/v1/jobs/{id}/cancel")))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        parse_empty(resp).await
    }

    // ---- cron ----

    pub async fn list_cron(&self) -> ClientResult<Vec<serde_json::Value>> {
        let env: ListResponse<serde_json::Value> = self.get_json("/v1/cron").await?;
        Ok(env.items)
    }

    pub async fn create_cron(&self, req: CreateCronRequest) -> ClientResult<serde_json::Value> {
        self.post_json("/v1/cron", &req).await
    }

    pub async fn get_cron(&self, id: &str) -> ClientResult<serde_json::Value> {
        self.get_json(&format!("/v1/cron/{id}")).await
    }

    pub async fn delete_cron(&self, id: &str) -> ClientResult<()> {
        self.delete(&format!("/v1/cron/{id}")).await
    }

    // ---- memory ----

    pub async fn list_memory(&self) -> ClientResult<Vec<MemorySummary>> {
        let env: ListResponse<MemorySummary> = self.get_json("/v1/memory").await?;
        Ok(env.items)
    }

    pub async fn store_memory(&self, req: StoreMemoryRequest) -> ClientResult<serde_json::Value> {
        self.post_json("/v1/memory", &req).await
    }

    pub async fn delete_memory(&self, id: &str) -> ClientResult<()> {
        self.delete(&format!("/v1/memory/{id}")).await
    }

    // ---- traces ----

    pub async fn get_trace(&self, id: &str) -> ClientResult<serde_json::Value> {
        self.get_json(&format!("/v1/traces/{id}")).await
    }

    // ---- skills / tools / channels ----

    pub async fn list_skills(&self) -> ClientResult<Vec<String>> {
        let env: ListResponse<String> = self.get_json("/v1/skills").await?;
        Ok(env.items)
    }

    pub async fn list_tools(&self) -> ClientResult<Vec<serde_json::Value>> {
        let env: ListResponse<serde_json::Value> = self.get_json("/v1/tools").await?;
        Ok(env.items)
    }

    pub async fn list_channels(&self) -> ClientResult<Vec<serde_json::Value>> {
        let env: ListResponse<serde_json::Value> = self.get_json("/v1/channels").await?;
        Ok(env.items)
    }

    // ---- approvals ----

    pub async fn list_approvals(&self) -> ClientResult<Vec<ApprovalRequest>> {
        let env: ListResponse<ApprovalRequest> = self.get_json("/v1/approvals").await?;
        Ok(env.items)
    }

    pub async fn resolve_approval(
        &self,
        call_id: &str,
        decision: ApprovalDecision,
    ) -> ClientResult<ResolveApprovalResponse> {
        let body = ResolveApprovalRequest { decision };
        self.post_json(&format!("/v1/approvals/{call_id}"), &body)
            .await
    }

    pub async fn subscribe_approvals(&self) -> ClientResult<SseStream<super::dto::ApprovalEvent>> {
        self.subscribe("/v1/approvals/stream").await
    }

    /// Open a long-lived request against an SSE endpoint. Timeout is
    /// disabled on the per-request builder so the stream may stay open
    /// for the lifetime of the TUI session.
    async fn subscribe<T>(&self, path: &str) -> ClientResult<SseStream<T>>
    where
        T: serde::de::DeserializeOwned + 'static,
    {
        let resp = self
            .inner
            .get(self.url(path))
            .bearer_auth(&self.token)
            .header("Accept", "text/event-stream")
            .timeout(Duration::MAX)
            .send()
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let resp = check_status(resp).await?;
        Ok(SseStream::from_response(resp))
    }
}

// ----- response helpers -----

async fn parse_json<R: DeserializeOwned>(resp: reqwest::Response) -> ClientResult<R> {
    let resp = check_status(resp).await?;
    resp.json::<R>()
        .await
        .map_err(|e| ClientError::Decode(e.to_string()))
}

async fn parse_empty(resp: reqwest::Response) -> ClientResult<()> {
    let _ = check_status(resp).await?;
    Ok(())
}

/// Map non-2xx responses to the typed error variants. Consumes the
/// response body on error so the caller can't accidentally read from
/// it twice.
async fn check_status(resp: reqwest::Response) -> ClientResult<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let message = extract_error_message(&body);
    Err(match status {
        StatusCode::UNAUTHORIZED => ClientError::Unauthorized,
        StatusCode::NOT_FOUND => ClientError::NotFound(message),
        StatusCode::BAD_REQUEST => ClientError::BadRequest(message),
        other => ClientError::Server {
            status: other.as_u16(),
            message,
        },
    })
}

/// The gateway returns `{ "error": "..." }` on failure; peel the field
/// if present, otherwise return the raw body.
fn extract_error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| body.to_string())
}

#[cfg(test)]
mod tests {
    //! Round-trip tests against a tiny in-process axum server. We don't
    //! re-implement the real gateway here — just enough handlers to
    //! exercise the `GatewayClient` request/response shaping, status
    //! mapping, and error-body extraction paths.

    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::{get, post, put};
    use serde_json::json;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    async fn spawn(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router.into_make_service()).await;
        });
        format!("http://{addr}")
    }

    fn client(base: &str) -> GatewayClient {
        GatewayClient::new(base, "test-token").expect("client")
    }

    #[tokio::test]
    async fn healthz_ok() {
        let app = Router::new().route("/healthz", get(|| async { StatusCode::OK }));
        let base = spawn(app).await;
        client(&base).healthz().await.expect("healthz");
    }

    #[tokio::test]
    async fn healthz_failure_surfaces_server_error() {
        let app = Router::new().route(
            "/healthz",
            get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "down") }),
        );
        let base = spawn(app).await;
        let err = client(&base).healthz().await.expect_err("must fail");
        match err {
            ClientError::Server { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Server, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_message_encodes_body_and_decodes_response() {
        let app = Router::new().route(
            "/v1/sessions/{session_id}/messages",
            post(
                |axum::extract::Path(session_id): axum::extract::Path<String>,
                 Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(session_id, "s-1");
                    assert_eq!(body["text"], "hello");
                    Json(json!({ "message_id": "m-42" }))
                },
            ),
        );
        let base = spawn(app).await;
        let out = client(&base)
            .post_message("s-1", "hello".into())
            .await
            .expect("post_message");
        assert_eq!(out.message_id, "m-42");
    }

    #[tokio::test]
    async fn list_sessions_unwraps_envelope() {
        let app = Router::new().route(
            "/v1/sessions",
            get(|| async {
                Json(json!({
                    "items": [
                        {
                            "id": "sess-1",
                            "user": { "id": "u1", "name": null, "channel": "tui" },
                            "channel": "tui",
                            "messages": [],
                            "created_at": "2026-01-01T00:00:00Z",
                            "last_active": "2026-01-01T00:00:00Z",
                            "state": {}
                        }
                    ]
                }))
            }),
        );
        let base = spawn(app).await;
        let out = client(&base).list_sessions().await.expect("list_sessions");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "sess-1");
    }

    #[tokio::test]
    async fn unauthorized_mapped() {
        let app = Router::new().route(
            "/v1/status",
            get(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "bad token" })),
                )
            }),
        );
        let base = spawn(app).await;
        let err = client(&base).status().await.expect_err("must fail");
        assert!(matches!(err, ClientError::Unauthorized), "got {err:?}");
    }

    #[tokio::test]
    async fn not_found_extracts_error_message() {
        let app = Router::new().route(
            "/v1/sessions/{id}",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "no such session" })),
                )
            }),
        );
        let base = spawn(app).await;
        let err = client(&base)
            .get_session("missing")
            .await
            .expect_err("must fail");
        match err {
            ClientError::NotFound(msg) => assert_eq!(msg, "no such session"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_config_round_trip() {
        let app = Router::new().route(
            "/v1/config",
            put(|Json(body): Json<serde_json::Value>| async move {
                assert_eq!(body["path"], "gateway.port");
                assert_eq!(body["value"], json!(7777));
                Json(json!({
                    "path": "gateway.port",
                    "written_to": "/tmp/aura.json",
                    "requires_restart": true,
                }))
            }),
        );
        let base = spawn(app).await;
        let out = client(&base)
            .set_config("gateway.port", json!(7777))
            .await
            .expect("set_config");
        assert_eq!(out.path, "gateway.port");
        assert!(out.requires_restart);
    }

    #[tokio::test]
    async fn resolve_approval_serializes_decision() {
        let app = Router::new().route(
            "/v1/approvals/{id}",
            post(
                |axum::extract::Path(id): axum::extract::Path<String>,
                 Json(body): Json<serde_json::Value>| async move {
                    Json(json!({ "call_id": id, "decision": body["decision"] }))
                },
            ),
        );
        let base = spawn(app).await;
        let out = client(&base)
            .resolve_approval("call-x", aura_tools::ApprovalDecision::Approve)
            .await
            .expect("resolve_approval");
        assert_eq!(out.call_id, "call-x");
        assert!(matches!(
            out.decision,
            aura_tools::ApprovalDecision::Approve
        ));
    }
}
