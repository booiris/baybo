//! HTTP+SSE-backed [`TuiTransport`] implementation.
//!
//! Wraps a [`GatewayClient`] and a local [`ApprovalQueue`]. User input
//! flows into `POST /v1/sessions/:id/messages`; outgoing events come
//! off the session SSE stream; approval events come off the separate
//! `/v1/approvals/stream` SSE and are reconciled into the local queue
//! so the TUI approval modal can drive resolutions back over HTTP.

use std::sync::Arc;

use async_trait::async_trait;
use aura_model::ContentBlock;
use aura_tools::{ApprovalQueue, ApprovalRequest};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tracing::{debug, warn};

use crate::tui::client::dto::{ApprovalEvent, SseEvent};
use crate::tui::client::http::GatewayClient;
use crate::tui::transport::{TransportEvent, TransportEventStream, TuiTransport};
use crate::{ChannelError, IncomingMessage, NoticeLevel, Result};

/// HTTP+SSE-backed [`TuiTransport`] implementation backed by a
/// [`GatewayClient`].
pub struct GatewayTransport {
    client: Arc<GatewayClient>,
    approval_queue: ApprovalQueue,
}

impl GatewayTransport {
    pub fn new(client: Arc<GatewayClient>) -> Self {
        let approval_queue = ApprovalQueue::new();
        // Install a resolver so the TUI's approval modal — which calls
        // `queue.resolve_head(decision)` — also POSTs the decision back
        // to the gateway. Without this, the server-side gate would time
        // out even after the user had picked "allow" or "deny" locally.
        let client_for_resolver = Arc::clone(&client);
        approval_queue.set_resolver(Arc::new(move |call_id, decision| {
            let client = Arc::clone(&client_for_resolver);
            tokio::spawn(async move {
                if let Err(e) = client.resolve_approval(&call_id, decision).await {
                    warn!(call_id, "approval resolution failed: {e}");
                }
            });
        }));
        Self {
            client,
            approval_queue,
        }
    }

    /// Access the underlying HTTP client — used by the slash/dashboard
    /// providers that also live in this module.
    pub fn client(&self) -> Arc<GatewayClient> {
        Arc::clone(&self.client)
    }
}

#[async_trait]
impl TuiTransport for GatewayTransport {
    async fn submit(&self, msg: IncomingMessage) -> Result<()> {
        // The gateway's POST body is plain text; concat any text blocks
        // (the TUI only ever builds single-text-block incoming messages
        // anyway, but be defensive about future multi-block inputs).
        let text = msg
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        self.client
            .post_message(&msg.message.session_id, text)
            .await
            .map_err(|e| ChannelError::Send(format!("post_message: {e}")))?;
        Ok(())
    }

    async fn subscribe(&self, session_id: &str) -> Result<TransportEventStream> {
        let session_stream = self
            .client
            .subscribe_session(session_id)
            .await
            .map_err(|e| ChannelError::Send(format!("session stream: {e}")))?;

        let approvals_stream = self
            .client
            .subscribe_approvals()
            .await
            .map_err(|e| ChannelError::Send(format!("approvals stream: {e}")))?;

        // Map SSE frames onto the TUI's transport-level events. Unknown
        // frames drop; transport errors surface as `Err` on the stream
        // so the event loop can log them without tearing down.
        let session_mapped = session_stream.filter_map(|item| async move {
            match item {
                Ok(SseEvent::Delta { text }) => Some(Ok(TransportEvent::StreamDelta(text))),
                Ok(SseEvent::Response { text }) => {
                    Some(Ok(TransportEvent::Response(vec![ContentBlock::Text(text)])))
                }
                Ok(SseEvent::Notice { level, text }) => Some(Ok(TransportEvent::Notice {
                    level: match level.as_str() {
                        "error" => NoticeLevel::Error,
                        _ => NoticeLevel::Warn,
                    },
                    text,
                })),
                Err(e) => Some(Err(ChannelError::Send(format!("session sse: {e}")))),
            }
        });

        let queue = self.approval_queue.clone();
        let target_session = session_id.to_owned();
        let approvals_mapped = approvals_stream.filter_map(move |item| {
            let queue = queue.clone();
            let target_session = target_session.clone();
            async move {
                match item {
                    Ok(ApprovalEvent::Added {
                        call_id,
                        session_id,
                        tool,
                        accesses,
                        params_preview,
                    }) => {
                        // The approval stream is gateway-wide (keyed by
                        // channel type), so drop events for other
                        // sessions — otherwise every TUI connected to
                        // the gateway would prompt for every session's
                        // approvals.
                        if session_id != target_session {
                            debug!(call_id, tool, other = %session_id, "approval from other session; ignoring");
                            return None;
                        }
                        // Mirror the server-side entry into the local
                        // queue so the TUI's approval modal picks it
                        // up. The oneshot responder is owned by
                        // whichever surface resolves it first.
                        debug!(call_id, tool, "approval added from gateway");
                        queue.enqueue_mirror(ApprovalRequest {
                            call_id,
                            session_id,
                            tool,
                            accesses,
                            params_preview,
                        });
                        Some(Ok(TransportEvent::ApprovalRequested))
                    }
                    Ok(ApprovalEvent::Resolved { call_id, decision }) => {
                        // Drop any local mirror so the TUI doesn't keep
                        // prompting for something another client just
                        // resolved.
                        let _ = queue.drop_call(&call_id);
                        Some(Ok(TransportEvent::ApprovalResolved { call_id, decision }))
                    }
                    Err(e) => {
                        warn!("approval sse: {e}");
                        Some(Err(ChannelError::Send(format!("approval sse: {e}"))))
                    }
                }
            }
        });

        // Merge both streams into a single transport stream; ordering
        // between the two sources doesn't matter — the session stream
        // is per-session, the approval stream is per-channel, and the
        // TUI handles them independently.
        let merged: BoxStream<'static, Result<TransportEvent>> =
            Box::pin(stream::select(session_mapped, approvals_mapped));
        Ok(merged)
    }

    fn approval_queue(&self) -> ApprovalQueue {
        self.approval_queue.clone()
    }
}
