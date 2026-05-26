use aura_channels::{AgentEvent, AgentOutput, OutgoingMessage};
use aura_model::SessionId;
use tracing::{debug, error, warn};

use super::Router;

impl Router {
    pub(super) async fn handle_agent_output(&self, output: AgentOutput) {
        let AgentOutput {
            session_id,
            user_id,
            channel,
            event,
        } = output;

        // `Message` is the only variant that carries user-visible prose
        // subject to policy egress — sanitize it before dispatch. `Delta`
        // chunks are intentionally exempt (incremental streaming; the final
        // `Message` is the authoritative sanitized egress per
        // `docs/modules/security.md`), and `Notice` is system-authored.
        let event = match event {
            AgentEvent::Message(outgoing) => {
                AgentEvent::Message(self.sanitize_outgoing(outgoing).await)
            }
            other => other,
        };

        let Some(channel_handle) = self.channels.get(&channel) else {
            debug!(
                channel = %channel,
                session_id = %session_id,
                "no channel installed for agent output"
            );
            return;
        };

        // Non-blocking fan-out: the channel `try_send`s to each
        // subscriber, drops on full (signalling Reset to the slow
        // peer), and detaches closed transports. No await — backpressure
        // is per-connection, not agent-wide.
        channel_handle.dispatch_agent(AgentOutput {
            session_id,
            user_id,
            channel,
            event,
        });
    }

    /// Run the security gateway over an outgoing message. Returns the
    /// (possibly mutated) message — if the session vanished or the gateway
    /// errored, the message is forwarded as-is; the gateway mutates in
    /// place even on error (redaction notice replaces the content) so we
    /// still send what it produced rather than the original.
    async fn sanitize_outgoing(&self, mut outgoing: OutgoingMessage) -> OutgoingMessage {
        let span = tracing::info_span!(
            "sanitize_outgoing",
            session_id = %outgoing.session_id,
            channel = %outgoing.channel,
        );
        let _guard = span.enter();

        let outgoing_session_id = SessionId::from(outgoing.session_id.as_str());
        let session = match self.session_manager.get(&outgoing_session_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                warn!(
                    session_id = %outgoing.session_id,
                    "session not found for outgoing sanitization, skipping security scan"
                );
                return outgoing;
            }
            Err(e) => {
                error!(
                    session_id = %outgoing.session_id,
                    error = %e,
                    "failed to load session for output sanitization"
                );
                return outgoing;
            }
        };

        if let Err(e) = self
            .security_gateway
            .sanitize_output(&mut outgoing, &session)
            .await
        {
            warn!(
                session_id = %outgoing.session_id,
                error = %e,
                "security gateway blocked or modified outgoing message"
            );
        }
        outgoing
    }
}
