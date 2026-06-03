//! Dispatch for [`aura_model::SystemSpawnRequest`] — one submodule per
//! variant:
//!
//! - [`subagent`] — child agent spawned by the `spawn_subagent` tool,
//!   with a bidirectional output channel and a watcher task waiting on
//!   terminal events
//!
//! The dispatcher itself ([`Router::handle_system_spawn`]) lives here
//! and simply fans out to the variant handlers; everything substantive
//! is in the submodules.
//!
//! Background compression is no longer a `SystemSpawnRequest` variant —
//! it runs as a detached in-actor pass on the parent's own `AgentLoop`
//! (see [`crate::runtime::agent_loop::AgentLoop::maybe_run_background_compression`]).

mod subagent;

use aura_model::SystemSpawnRequest;

use super::Router;

impl Router {
    /// Materialise a [`SystemSpawnRequest`] into a fresh session +
    /// actor, then deliver the kickoff message into its mailbox.
    ///
    /// Symmetric to [`Router::handle_cron_trigger`]:
    ///   - **Session**: created via `create_spawned_session`, so the
    ///     row is lineaged to the parent.
    ///   - **Cancel parent**: the request carries the originating
    ///     parent actor's `actor_token`; the spawned child's
    ///     `actor_token` derives as a grandchild. Cancelling the
    ///     parent therefore cascades into the child via the
    ///     `tokio_util` token tree, with no Shutdown mailbox dance.
    ///
    /// The spawned actor is intentionally NOT registered with the
    /// supervisor — subagents are one-shot and registering would just
    /// accumulate dangling handles.
    pub(super) async fn handle_system_spawn(
        &mut self,
        request: SystemSpawnRequest,
    ) -> anyhow::Result<()> {
        match request {
            SystemSpawnRequest::Subagent {
                parent_session_id,
                parent_job_id,
                parent_span_id,
                parent_actor_token,
                request,
                result_tx,
            } => {
                self.handle_subagent_spawn(
                    parent_session_id,
                    parent_job_id,
                    parent_span_id,
                    parent_actor_token,
                    *request,
                    result_tx,
                )
                .await
            }
        }
    }
}
