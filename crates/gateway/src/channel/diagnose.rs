//! Admin-side diagnose round-trip over the channel WS.
//!
//! The admin endpoint `GET /v1/admin/channels/<type>/diagnose` issues a
//! [`Frame::DiagnoseRequest`] to the currently-connected sidecar for
//! `<type>` and waits for the matching [`Frame::DiagnoseReply`] before
//! responding. The router below tracks pending request_ids so the
//! inbound loop can resolve them when the sidecar replies.
//!
//! Capability gate: only sidecars that advertise the `"diagnose"`
//! capability on `Register` see the request. A request issued against a
//! sidecar that doesn't claim it is rejected with a typed error before
//! any frame is sent — better than silently waiting on a hook the peer
//! doesn't implement.

use std::sync::Arc;
use std::time::Duration;

use aura_channels::wire::{DiagnoseCheck, Frame};
use aura_model::ChannelType;
use dashmap::DashMap;
use thiserror::Error;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::control::{ChannelControlError, ChannelControlRegistry};

/// Per-channel-type capability set captured at register time. Wrapped
/// in a newtype so callers in other crates don't have to depend on
/// `dashmap` directly (the only state shared across the WS task and
/// the admin endpoint that needed concurrent map access).
#[derive(Default)]
pub struct ChannelCapabilities {
    inner: DashMap<ChannelType, Vec<String>>,
}

impl ChannelCapabilities {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, channel_type: ChannelType, capabilities: Vec<String>) {
        self.inner.insert(channel_type, capabilities);
    }

    pub fn forget(&self, channel_type: &ChannelType) {
        self.inner.remove(channel_type);
    }

    pub fn supports(&self, channel_type: &ChannelType, capability: &str) -> bool {
        self.inner
            .get(channel_type)
            .is_some_and(|caps| caps.iter().any(|c| c == capability))
    }
}

/// Default round-trip cap. Long enough that a sidecar's diagnose hook
/// can run a real probe (Lark wants to call `bot.getMe()` and bounce a
/// test message off the WSS); short enough that an operator hitting
/// the endpoint sees a definitive answer.
pub const DIAGNOSE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Error)]
pub enum DiagnoseError {
    #[error("no sidecar connected for channel_type '{0}'")]
    NotConnected(String),
    #[error("sidecar for channel_type '{0}' has not advertised the 'diagnose' capability")]
    CapabilityMissing(String),
    #[error("sidecar disconnected before replying")]
    Disconnected,
    #[error("sidecar did not reply within {0:?}")]
    Timeout(Duration),
    #[error("sidecar replied with error: {0}")]
    SidecarError(String),
    #[error("control plane error: {0}")]
    Control(#[from] ChannelControlError),
}

#[derive(Debug, Clone)]
pub struct DiagnoseReport {
    pub bot_id: String,
    pub checks: Vec<DiagnoseCheck>,
}

#[derive(Default)]
pub struct DiagnoseRouter {
    pending: DashMap<String, oneshot::Sender<DiagnoseDelivery>>,
}

enum DiagnoseDelivery {
    Reply { checks: Vec<DiagnoseCheck> },
    Error(String),
}

impl std::fmt::Debug for DiagnoseDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiagnoseDelivery::Reply { checks } => {
                f.debug_struct("Reply").field("checks", checks).finish()
            }
            DiagnoseDelivery::Error(s) => f.debug_tuple("Error").field(s).finish(),
        }
    }
}

impl DiagnoseRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a pending request with the sidecar's reply. Called by
    /// the inbound loop when a `Frame::DiagnoseReply` lands. Unknown
    /// `request_id`s are dropped silently — typical when a request
    /// timed out earlier and the sidecar's late reply arrives anyway.
    pub fn resolve(
        &self,
        request_id: &str,
        ok: bool,
        checks: Vec<DiagnoseCheck>,
        error: Option<String>,
    ) {
        let Some((_, tx)) = self.pending.remove(request_id) else {
            tracing::debug!(%request_id, "diagnose reply for unknown request; dropping");
            return;
        };
        let delivery = if ok {
            DiagnoseDelivery::Reply { checks }
        } else {
            DiagnoseDelivery::Error(error.unwrap_or_else(|| "unspecified".into()))
        };
        let _ = tx.send(delivery);
    }

    /// Drop every pending waiter when a sidecar disconnects. Each
    /// gets a `Disconnected` outcome rather than waiting for the
    /// timeout to fire — admin endpoints surface the closed-pipe
    /// reason immediately.
    pub fn drain_for_disconnect(&self) {
        let keys: Vec<String> = self.pending.iter().map(|e| e.key().clone()).collect();
        for k in keys {
            if let Some((_, tx)) = self.pending.remove(&k) {
                let _ = tx.send(DiagnoseDelivery::Error("disconnected".into()));
            }
        }
    }
}

/// Run a single diagnose round-trip against the sidecar registered
/// for `channel_type`. Returns the assembled report, or a typed error
/// the admin endpoint can map to an HTTP status.
pub async fn request_diagnose(
    control: &Arc<ChannelControlRegistry>,
    router: &Arc<DiagnoseRouter>,
    capability_advertised: bool,
    channel_type: &ChannelType,
    bot_id: String,
) -> Result<DiagnoseReport, DiagnoseError> {
    if !capability_advertised {
        return Err(DiagnoseError::CapabilityMissing(channel_type.to_string()));
    }
    if !control.is_connected(channel_type) {
        return Err(DiagnoseError::NotConnected(channel_type.to_string()));
    }

    let request_id = Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();
    router.pending.insert(request_id.clone(), tx);

    let frame = Frame::DiagnoseRequest {
        request_id: request_id.clone(),
        bot_id: bot_id.clone(),
    };
    if let Err(err) = control.send(channel_type, frame).await {
        // Pending entry would otherwise leak until the timeout fires.
        router.pending.remove(&request_id);
        return Err(DiagnoseError::from(err));
    }

    match tokio::time::timeout(DIAGNOSE_TIMEOUT, rx).await {
        Ok(Ok(DiagnoseDelivery::Reply { checks })) => Ok(DiagnoseReport { bot_id, checks }),
        Ok(Ok(DiagnoseDelivery::Error(reason))) => match reason.as_str() {
            "disconnected" => Err(DiagnoseError::Disconnected),
            other => Err(DiagnoseError::SidecarError(other.to_string())),
        },
        Ok(Err(_)) => {
            // Sender dropped — equivalent to disconnect.
            Err(DiagnoseError::Disconnected)
        }
        Err(_) => {
            // Timeout. Best-effort cleanup of the pending entry.
            router.pending.remove(&request_id);
            Err(DiagnoseError::Timeout(DIAGNOSE_TIMEOUT))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_channels::wire::{DiagnoseCheck, DiagnoseStatus};

    #[test]
    fn resolve_unknown_id_is_silent_drop() {
        let router = DiagnoseRouter::new();
        // No pending entry; this must not panic.
        router.resolve("never-issued", true, vec![], None);
    }

    #[tokio::test]
    async fn drain_for_disconnect_completes_pending_with_disconnected() {
        let router = DiagnoseRouter::new();
        let (tx, rx) = oneshot::channel();
        router.pending.insert("rid".into(), tx);
        router.drain_for_disconnect();
        match rx.await {
            Ok(DiagnoseDelivery::Error(reason)) => assert_eq!(reason, "disconnected"),
            other => panic!("unexpected delivery: {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_with_checks_reaches_waiter() {
        let router = DiagnoseRouter::new();
        let (tx, rx) = oneshot::channel();
        router.pending.insert("rid".into(), tx);
        router.resolve(
            "rid",
            true,
            vec![DiagnoseCheck {
                name: "ws".into(),
                status: DiagnoseStatus::Ok,
                detail: "connected".into(),
            }],
            None,
        );
        match rx.await {
            Ok(DiagnoseDelivery::Reply { checks }) => {
                assert_eq!(checks.len(), 1);
                assert_eq!(checks[0].name, "ws");
            }
            other => panic!("unexpected delivery: {other:?}"),
        }
    }
}
