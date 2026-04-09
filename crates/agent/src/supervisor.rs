use std::collections::HashMap;

use aura_core::OutgoingMessage;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::actor::AgentMessage;

/// Handle to communicate with a running AgentActor.
pub struct ActorHandle {
    pub sender: mpsc::Sender<AgentMessage>,
}

/// Manages AgentActor instances, one per active session.
pub struct AgentSupervisor {
    actors: HashMap<String, ActorHandle>,
    response_tx: mpsc::Sender<OutgoingMessage>,
}

impl AgentSupervisor {
    pub fn new(response_tx: mpsc::Sender<OutgoingMessage>) -> Self {
        Self {
            actors: HashMap::new(),
            response_tx,
        }
    }

    /// Send a message to the actor for a given session.
    /// Returns false if the actor doesn't exist.
    pub async fn route(&self, session_id: &str, message: AgentMessage) -> bool {
        if let Some(handle) = self.actors.get(session_id) {
            if let Err(e) = handle.sender.send(message).await {
                tracing::warn!(
                    session_id,
                    error = %e,
                    "failed to send message to actor"
                );
                return false;
            }
            true
        } else {
            false
        }
    }

    /// Register a new actor handle.
    pub fn register(&mut self, session_id: String, sender: mpsc::Sender<AgentMessage>) {
        debug!(session_id = %session_id, "registering actor");
        self.actors.insert(session_id, ActorHandle { sender });
    }

    /// Get the response channel sender (for creating new actors).
    pub fn response_tx(&self) -> &mpsc::Sender<OutgoingMessage> {
        &self.response_tx
    }

    /// Shut down all actors gracefully.
    pub async fn shutdown_all(&self) {
        info!(count = self.actors.len(), "shutting down all actors");
        for (session_id, handle) in &self.actors {
            if let Err(e) = handle.sender.send(AgentMessage::Shutdown).await {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "failed to send shutdown to actor"
                );
            }
        }
    }
}
