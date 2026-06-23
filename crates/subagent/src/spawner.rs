//! The subagent spawn capability the `spawn_subagent` tool depends on.
//!
//! Like [`crate::SubagentDispatchLimiter`], this trait lives in the leaf
//! crate so the tool can depend on the capability without a cycle: the
//! tool holds an `Arc<dyn SubagentSpawner>`, and `baybo-agent` provides the
//! actor-backed impl (which builds a real `AgentActor` for the child).
//! `baybo-subagent` depends on neither the runtime nor `baybo-tools`, so
//! neither direction closes a loop.

use async_trait::async_trait;
use baybo_model::{SubagentParentContext, SubagentResult, SubagentSpawnRequest};

/// Launches a child subagent and returns the result the tool renders.
///
/// One call covers both regimes (the `request.background` flag selects):
/// a **foreground** spawn blocks until the child reaches a terminal state
/// and returns that `SubagentResult`; a **background** spawn returns the
/// dispatch ack immediately while the child runs on, its terminal later
/// escorted to the parent as a notification turn. The caller reserves a
/// fan-out slot before calling and the implementation releases it on the
/// child's terminal — except when `spawn` is never reached (e.g. the impl
/// is not yet wired), where the caller releases.
#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    async fn spawn(
        &self,
        parent: SubagentParentContext,
        request: SubagentSpawnRequest,
    ) -> SubagentResult;
}
