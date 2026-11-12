//! The subagent spawn capability the `spawn_subagent` tool depends on.
//!
//! Like [`crate::SubagentDispatchLimiter`], this trait lives in the leaf
//! crate so the tool can depend on the capability without a cycle: the
//! tool holds an `Arc<dyn SubagentSpawner>`, and `baybo-agent` provides the
//! actor-backed impl (which builds a real `AgentActor` for the child).
//! `baybo-subagent` depends on neither the runtime nor `baybo-tools`, so
//! neither direction closes a loop.

use async_trait::async_trait;
use baybo_model::{SessionId, SubagentParentContext, SubagentResult, SubagentSpawnRequest};
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

/// Launches a child subagent and hands its run back to the tool.
///
/// The spawner owns what is particular to a subagent — resolving or creating
/// the child session, building its actor or external process, and wiring its
/// cancellation. It does not decide how long the parent waits: the tool runs
/// the returned [`SubagentRun`] through `baybo_tools::background::run`, which
/// owns the foreground window, the conversion to background and delivery,
/// exactly as it does for every other background-capable tool.
///
/// The caller reserves a fan-out slot before calling. The implementation
/// releases it when the run's `terminal` completes, or at once when it
/// returns [`SpawnedSubagent::Failed`] — except when `spawn` is never reached
/// (e.g. the impl is not yet wired), where the caller releases.
#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    async fn spawn(
        &self,
        parent: SubagentParentContext,
        request: SubagentSpawnRequest,
    ) -> SpawnedSubagent;
}

/// What [`SubagentSpawner::spawn`] produced.
pub enum SpawnedSubagent {
    /// The child exists and its run is ready to be driven.
    Running(SubagentRun),
    /// The child never started; this is the result to report.
    Failed(SubagentResult),
}

/// A child subagent's run, from the parent's side.
pub struct SubagentRun {
    pub child_session_id: SessionId,
    /// Stops the child. Tripping it makes `terminal` resolve promptly.
    pub cancel: CancellationToken,
    /// Starts the child's work when first polled and resolves with its
    /// terminal result. Anchored to the process when the parent may hand it
    /// to the background (`SubagentParentContext::background_eligible`), so
    /// it does not die with the dispatching turn.
    pub terminal: BoxFuture<'static, SubagentResult>,
}
