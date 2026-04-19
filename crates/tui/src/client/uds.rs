//! UDS-backed HTTP transport for the gateway's channel listener.
//!
//! `reqwest` has no first-party Unix-socket support, so we drive a
//! `hyper` HTTP/1 client directly over [`tokio::net::UnixStream`]. Each
//! call opens a fresh connection, performs the request, and drops the
//! connection on completion — the channel surface is low-traffic
//! (messages + long-lived SSE) so keep-alive would be a minor win at a
//! large code-complexity cost.
//!
//! The TUI presents its per-install effective PSK on every request via
//! the [`TUI_PSK_HEADER`] header; the gateway side is implemented in
//! `aura_gateway::auth_channel`.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use aura_gateway_auth::TUI_PSK_HEADER;
use bytes::Bytes;
use futures::Stream;
use http_body_util::{BodyExt, Full};
use hyper::Method;
use hyper::Request;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use super::dto::{ClientError, ClientResult};
use super::sse::{SseByteStream, SseStream};

/// Shared UDS transport. Cheap to clone — the socket path is wrapped in
/// an `Arc` and the hex-encoded PSK is small.
#[derive(Clone)]
pub struct UdsTransport {
    socket_path: Arc<PathBuf>,
    psk_hex: Arc<str>,
}

impl UdsTransport {
    /// Build a transport bound to `socket_path`, presenting `psk` hex-
    /// encoded on the `x-aura-tui-secret` header of every request.
    pub fn new(socket_path: PathBuf, psk: [u8; 32]) -> Self {
        Self {
            socket_path: Arc::new(socket_path),
            psk_hex: Arc::from(hex::encode(psk)),
        }
    }

    pub fn socket_path(&self) -> &Path {
        self.socket_path.as_path()
    }

    /// Execute a request over a fresh UDS connection. The returned
    /// response is detached from the driver via `into_parts` so the
    /// caller can read the body for as long as the driver future keeps
    /// running — we spawn that future onto the tokio runtime and let it
    /// exit naturally when the body completes or the client drops.
    ///
    /// For streaming endpoints, wrap the body + driver together so
    /// cancelling the stream tears down both.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        extra_accept: Option<&'static str>,
    ) -> ClientResult<hyper::Response<Incoming>> {
        let stream = UnixStream::connect(self.socket_path.as_path())
            .await
            .map_err(|e| {
                ClientError::Transport(format!("connect {}: {e}", self.socket_path.display()))
            })?;
        let io = TokioIo::new(stream);

        let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| ClientError::Transport(format!("handshake: {e}")))?;
        let driver: JoinHandle<()> = tokio::spawn(async move {
            let _ = conn.await;
        });

        // `host` is cosmetic on UDS but HTTP/1.1 requires it.
        let mut req_builder = Request::builder()
            .method(&method)
            .uri(path)
            .header("host", "aura.local")
            .header(TUI_PSK_HEADER, self.psk_hex.as_ref());
        if let Some(accept) = extra_accept {
            req_builder = req_builder.header("accept", accept);
        }
        if !body.is_empty() {
            req_builder = req_builder.header("content-type", "application/json");
        }
        let req = req_builder
            .body(Full::new(body))
            .map_err(|e| ClientError::Transport(format!("build request: {e}")))?;

        let resp = sender
            .send_request(req)
            .await
            .map_err(|e| ClientError::Transport(format!("send_request: {e}")))?;

        // Stash the driver with the response so the body can be read
        // without the connection shutting down under our feet. The
        // driver is detached from the JoinHandle; we rely on hyper's
        // semantics: the driver exits on its own when the body is
        // fully consumed or the sender is dropped.
        drop(driver); // detached; don't await it

        Ok(resp)
    }

    async fn request_json<R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
    ) -> ClientResult<R> {
        let resp = self.send(method, path, body, None).await?;
        let (parts, body) = resp.into_parts();
        let bytes = body
            .collect()
            .await
            .map_err(|e| ClientError::Transport(format!("read body: {e}")))?
            .to_bytes();
        map_status(parts.status, &bytes)?;
        serde_json::from_slice::<R>(&bytes).map_err(|e| ClientError::Decode(e.to_string()))
    }

    async fn request_empty(&self, method: Method, path: &str, body: Bytes) -> ClientResult<()> {
        let resp = self.send(method, path, body, None).await?;
        let (parts, body) = resp.into_parts();
        let bytes = body
            .collect()
            .await
            .map_err(|e| ClientError::Transport(format!("read body: {e}")))?
            .to_bytes();
        map_status(parts.status, &bytes)?;
        Ok(())
    }

    pub async fn get_json<R: DeserializeOwned>(&self, path: &str) -> ClientResult<R> {
        self.request_json(Method::GET, path, Bytes::new()).await
    }

    pub async fn post_json<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ClientResult<R> {
        let bytes = serde_json::to_vec(body).map_err(|e| ClientError::Decode(e.to_string()))?;
        self.request_json(Method::POST, path, Bytes::from(bytes))
            .await
    }

    pub async fn post_empty(&self, path: &str) -> ClientResult<()> {
        self.request_empty(Method::POST, path, Bytes::new()).await
    }

    pub async fn delete(&self, path: &str) -> ClientResult<()> {
        self.request_empty(Method::DELETE, path, Bytes::new()).await
    }

    pub async fn healthz(&self) -> ClientResult<()> {
        self.request_empty(Method::GET, "/healthz", Bytes::new())
            .await
    }

    /// Open a long-lived SSE subscription against `path`. The returned
    /// stream owns both the body and the underlying hyper driver task;
    /// dropping it cancels the request.
    pub async fn subscribe<T>(&self, path: &str) -> ClientResult<SseStream<T>>
    where
        T: DeserializeOwned + 'static,
    {
        let resp = self
            .send(Method::GET, path, Bytes::new(), Some("text/event-stream"))
            .await?;
        let (parts, body) = resp.into_parts();
        if !parts.status.is_success() {
            // Consume the error body for a useful message.
            let bytes = body
                .collect()
                .await
                .map_err(|e| ClientError::Transport(format!("read body: {e}")))?
                .to_bytes();
            map_status(parts.status, &bytes)?;
            // map_status returns Err on non-2xx; unreachable.
            return Err(ClientError::Server {
                status: parts.status.as_u16(),
                message: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        let byte_stream: SseByteStream = Box::pin(IncomingBytesStream::new(body));
        Ok(SseStream::from_bytes_stream(byte_stream))
    }
}

/// Adapts a hyper [`Incoming`] body into a `Stream<Item = ClientResult<Bytes>>`.
/// Trailer frames are dropped — the SSE parser only looks at data.
struct IncomingBytesStream {
    body: Incoming,
}

impl IncomingBytesStream {
    fn new(body: Incoming) -> Self {
        Self { body }
    }
}

impl Stream for IncomingBytesStream {
    type Item = ClientResult<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use hyper::body::Body;
        loop {
            match Pin::new(&mut self.body).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(bytes) => return Poll::Ready(Some(Ok(bytes))),
                    // Non-data frame (trailers). Keep polling for the
                    // next one rather than stalling the stream.
                    Err(_trailers) => continue,
                },
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(ClientError::Transport(e.to_string()))));
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Map non-2xx HTTP status + error body onto a typed [`ClientError`].
fn map_status(status: StatusCode, body: &[u8]) -> ClientResult<()> {
    if status.is_success() {
        return Ok(());
    }
    let message = extract_error_message(body);
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

fn extract_error_message(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::extract::Path as AxumPath;
    use axum::routing::{get, post};
    use futures::StreamExt;
    use hyper_util::rt::TokioExecutor;
    use hyper_util::server::conn::auto::Builder as AutoBuilder;
    use serde_json::json;
    use std::convert::Infallible;
    use tokio::net::UnixListener;
    use tower::Service;

    /// Bind a UDS, serve `router`, return (tempdir, socket_path). The
    /// tempdir must stay alive for the lifetime of the test — dropping
    /// it removes the socket.
    async fn spawn_uds(router: Router) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let router = router.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = hyper::service::service_fn(
                        move |req: hyper::Request<hyper::body::Incoming>| {
                            let mut router = router.clone();
                            async move {
                                let resp = router.call(req).await?;
                                Ok::<_, Infallible>(resp)
                            }
                        },
                    );
                    let _ = AutoBuilder::new(TokioExecutor::new())
                        .serve_connection(io, service)
                        .await;
                });
            }
        });
        (dir, socket_path)
    }

    fn transport(socket_path: PathBuf) -> UdsTransport {
        UdsTransport::new(socket_path, [0u8; 32])
    }

    #[tokio::test]
    async fn get_json_round_trip() {
        let app = Router::new().route("/v1/ping", get(|| async { Json(json!({ "ok": true })) }));
        let (_dir, socket) = spawn_uds(app).await;
        let t = transport(socket);
        let v: serde_json::Value = t.get_json("/v1/ping").await.expect("get_json");
        assert_eq!(v, json!({ "ok": true }));
    }

    #[tokio::test]
    async fn post_json_round_trip() {
        let app = Router::new().route(
            "/v1/echo/{id}",
            post(
                |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| async move {
                    Json(json!({ "id": id, "echo": body["text"] }))
                },
            ),
        );
        let (_dir, socket) = spawn_uds(app).await;
        let t = transport(socket);
        let out: serde_json::Value = t
            .post_json("/v1/echo/abc", &json!({ "text": "hello" }))
            .await
            .expect("post_json");
        assert_eq!(out["id"], "abc");
        assert_eq!(out["echo"], "hello");
    }

    #[tokio::test]
    async fn healthz_maps_status() {
        let app = Router::new().route("/healthz", get(|| async { StatusCode::OK }));
        let (_dir, socket) = spawn_uds(app).await;
        let t = transport(socket);
        t.healthz().await.expect("healthz");
    }

    #[tokio::test]
    async fn not_found_extracts_error_message() {
        let app = Router::new().route(
            "/v1/missing",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "no such thing" })),
                )
            }),
        );
        let (_dir, socket) = spawn_uds(app).await;
        let t = transport(socket);
        let err: ClientResult<serde_json::Value> = t.get_json("/v1/missing").await;
        match err {
            Err(ClientError::NotFound(msg)) => assert_eq!(msg, "no such thing"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_streams_sse_frames() {
        use axum::response::sse::{Event, Sse};
        use futures::stream;
        let app = Router::new().route(
            "/v1/stream",
            get(|| async {
                let events = stream::iter(vec![
                    Ok::<_, Infallible>(
                        Event::default()
                            .event("delta")
                            .data(r#"{"kind":"delta","text":"hi"}"#),
                    ),
                    Ok::<_, Infallible>(Event::default().event("end").data("")),
                ]);
                Sse::new(events)
            }),
        );
        let (_dir, socket) = spawn_uds(app).await;
        let t = transport(socket);
        let mut s = t
            .subscribe::<crate::client::dto::SseEvent>("/v1/stream")
            .await
            .expect("subscribe");
        let first = s.next().await.expect("first frame").expect("decode");
        match first {
            crate::client::dto::SseEvent::Delta { text } => assert_eq!(text, "hi"),
            other => panic!("expected Delta, got {other:?}"),
        }
    }
}
