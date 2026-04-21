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
pub struct Channel {
    channel_type: ChannelType,
    output_tx: mpsc::Sender<AgentOutput>,
    approval_gate: Option<Arc<dyn ApprovalGate>>,
}

impl Channel {
    pub fn new(
        channel_type: ChannelType,
        output_tx: mpsc::Sender<AgentOutput>,
        approval_gate: Option<Arc<dyn ApprovalGate>>,
    ) -> Self {
        Self {
            channel_type,
            output_tx,
            approval_gate,
        }
    }

    pub fn channel_type(&self) -> &ChannelType {
        &self.channel_type
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
