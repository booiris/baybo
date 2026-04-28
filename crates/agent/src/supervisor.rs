use std::collections::HashMap;
use std::time::Duration;

use aura_channels::AgentOutput;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::actor::AgentMessage;

/// Handle to communicate with a running AgentActor.
pub struct ActorHandle {
    pub sender: mpsc::Sender<AgentMessage>,
}

/// Manages AgentActor instances, one per active session, plus a
/// shared list of every actor `JoinHandle` we have spawned so the
/// process-exit path can await them gracefully instead of aborting.
pub struct AgentSupervisor {
    actors: HashMap<String, ActorHandle>,
    /// Every actor's `JoinHandle`, including ones that aren't keyed
    /// by `session_id` (e.g. one-shot cron actors). Finished entries
    /// are pruned opportunistically on `register` / `track` so the
    /// vec doesn't grow unbounded across long-running gateways.
    join_handles: Vec<JoinHandle<()>>,
    response_tx: mpsc::Sender<AgentOutput>,
}

impl AgentSupervisor {
    pub fn new(response_tx: mpsc::Sender<AgentOutput>) -> Self {
        Self {
            actors: HashMap::new(),
            join_handles: Vec::new(),
            response_tx,
        }
    }

    /// Send a message to the actor for a given session.
    /// Returns false if the actor doesn't exist.
    pub async fn route(&self, session_id: &str, message: AgentMessage) -> bool {
        if let Some(handle) = self.actors.get(session_id) {
            if let Err(e) = handle.sender.send(message).await {
                warn!(
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

    /// Register a session-keyed actor and track its JoinHandle for
    /// graceful shutdown.
    pub fn register(
        &mut self,
        session_id: String,
        sender: mpsc::Sender<AgentMessage>,
        handle: JoinHandle<()>,
    ) {
        debug!(session_id = %session_id, "registering actor");
        self.actors.insert(session_id, ActorHandle { sender });
        self.track(handle);
    }

    /// Track a `JoinHandle` without registering a session-keyed
    /// sender. Used by the cron-fire path: each cron actor is
    /// one-shot (no follow-up traffic) so it doesn't need to live in
    /// `actors`, but we still want shutdown to await its in-flight
    /// trigger.
    pub fn track(&mut self, handle: JoinHandle<()>) {
        self.compact_join_handles();
        self.join_handles.push(handle);
    }

    fn compact_join_handles(&mut self) {
        self.join_handles.retain(|h| !h.is_finished());
    }

    /// Get the response channel sender (for creating new actors).
    pub fn response_tx(&self) -> &mpsc::Sender<AgentOutput> {
        &self.response_tx
    }

    /// Send `AgentMessage::Shutdown` to every registered actor and
    /// await every tracked `JoinHandle` for up to `grace`. After the
    /// grace window any still-running task is left for the runtime's
    /// `TaskTracker` to abort.
    pub async fn shutdown_all(&mut self, grace: Duration) {
        info!(
            registered = self.actors.len(),
            tracked = self.join_handles.len(),
            ?grace,
            "shutting down all actors"
        );
        for (session_id, handle) in &self.actors {
            if let Err(e) = handle.sender.send(AgentMessage::Shutdown).await {
                warn!(
                    session_id = %session_id,
                    error = %e,
                    "failed to send shutdown to actor"
                );
            }
        }
        let handles = std::mem::take(&mut self.join_handles);
        let drained = handles.len();
        let join_all = async move {
            for h in handles {
                let _ = h.await;
            }
        };
        match tokio::time::timeout(grace, join_all).await {
            Ok(()) => {
                debug!(count = drained, "all actor join handles drained");
            }
            Err(_) => {
                warn!(
                    count = drained,
                    ?grace,
                    "grace window elapsed before all actor join handles drained"
                );
            }
        }
    }
}
