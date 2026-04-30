//! Trait + RPC types for talking to the browser sidecar. The concrete
//! impl lives in `aura-gateway` (it owns the registered WebSocket
//! connection); `aura-tools` only sees this trait so it doesn't take
//! a transitive dependency on the gateway crate.

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::ToolError;

/// What the gateway-side WS client returns when an RPC fails.
#[derive(Debug, Error)]
pub enum BrowserRpcError {
    #[error("browser sidecar is not connected")]
    Disconnected,
    #[error("browser RPC timed out after {0:?}")]
    Timeout(Duration),
    #[error("browser RPC cancelled")]
    Cancelled,
    /// The sidecar returned a structured error frame.
    #[error("browser sidecar error: {code}: {message}")]
    Sidecar { code: String, message: String },
    /// Frame encode/decode or socket I/O failure.
    #[error("browser RPC transport error: {0}")]
    Transport(String),
}

impl From<BrowserRpcError> for ToolError {
    fn from(e: BrowserRpcError) -> Self {
        match e {
            BrowserRpcError::Timeout(d) => {
                ToolError::Timeout(format!("browser sidecar exceeded {d:?}"))
            }
            BrowserRpcError::Cancelled => ToolError::Execution("cancelled".into()),
            other => ToolError::Execution(other.to_string()),
        }
    }
}

/// Talks to the browser sidecar. One implementation per gateway
/// instance; tools clone the `Arc` and call through it concurrently.
///
/// `context_id` is the Aura `session_id` — the sidecar uses it to
/// route per-session `BrowserContext`s so two sessions never share
/// cookies / localStorage / etc.
#[async_trait]
pub trait BrowserSidecarClient: Send + Sync {
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
        context_id: &str,
        timeout: Duration,
        cancellation_token: CancellationToken,
    ) -> Result<serde_json::Value, BrowserRpcError>;
}
