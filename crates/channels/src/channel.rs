//! Concrete channel handle stored in [`ChannelRegistry`].
//!
//! Replaces the old `ChannelAdapter` trait. Each registered channel is
//! a `Channel` value that forwards [`AgentOutput`] over an outbound
//! mpsc — the transport (today: the gateway WS sidecar pump) owns the
//! receiver and encodes frames onto the wire. Collapsing the trait
//! keeps the router free of dynamic dispatch and lets the channel
//! registry stay agnostic of any wire format.

use std::sync::Arc;

use aura_model::ChannelType;
use aura_tools::ApprovalGate;
use tokio::sync::mpsc;

use crate::{AgentOutput, ChannelError, Result};

/// Handle to a live channel. Cloneable via `Arc<Channel>`; one instance
/// per registered channel in the [`ChannelRegistry`].
///
/// A channel is either a **sidecar** (serves every session of its
/// channel type — `owned_session = None`) or a **session-scoped
/// client** that is attached to exactly one session (`owned_session =
/// Some`). The built-in TUI uses the session-scoped variant so
/// multiple TUI processes can share a gateway without colliding on
/// the `ChannelType::tui()` registration slot.
pub struct Channel {
    channel_type: ChannelType,
    output_tx: mpsc::Sender<AgentOutput>,
    approval_gate: Option<Arc<dyn ApprovalGate>>,
    owned_session: Option<String>,
}

impl Channel {
    /// Build a sidecar channel: claims the whole `channel_type` slot
    /// in the registry (1:1) and handles output for every session of
    /// that type.
    pub fn new(
        channel_type: ChannelType,
        output_tx: mpsc::Sender<AgentOutput>,
        approval_gate: Option<Arc<dyn ApprovalGate>>,
    ) -> Self {
        Self {
            channel_type,
            output_tx,
            approval_gate,
            owned_session: None,
        }
    }

    /// Build a session-scoped client: pinned to exactly one session.
    /// Multiple session-scoped clients of the same channel type may
    /// coexist as long as their `session_id`s differ.
    pub fn new_session_scoped(
        channel_type: ChannelType,
        session_id: String,
        output_tx: mpsc::Sender<AgentOutput>,
        approval_gate: Option<Arc<dyn ApprovalGate>>,
    ) -> Self {
        Self {
            channel_type,
            output_tx,
            approval_gate,
            owned_session: Some(session_id),
        }
    }

    pub fn channel_type(&self) -> &ChannelType {
        &self.channel_type
    }

    /// Session this channel is pinned to, if any. `None` marks a
    /// sidecar that handles every session of its channel type.
    pub fn owned_session(&self) -> Option<&str> {
        self.owned_session.as_deref()
    }

    /// Approval gate registered at construction time, if any. Handed to
    /// [`ChannelRegistry`] so it can populate the shared gate map.
    pub fn approval_gate(&self) -> Option<Arc<dyn ApprovalGate>> {
        self.approval_gate.clone()
    }

    /// Forward one agent output to the channel's transport. Fails if
    /// the transport has dropped its receiver (e.g. the WS sidecar
    /// disconnected between registry lookup and this send).
    pub async fn send(&self, output: AgentOutput) -> Result<()> {
        self.output_tx
            .send(output)
            .await
            .map_err(|_| ChannelError::Config("channel transport closed".into()))
    }
}
