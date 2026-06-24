//! Per-actor state split into durable and volatile halves.
//!
//! The split is a type-level statement of what survives an actor's
//! lifetime versus what does not:
//!
//! - [`DurableActorState`] is the **source of truth** for everything an
//!   actor needs to be reconstructed losslessly after eviction. Each
//!   field must be persisted to a store (the session row, the transcript
//!   table, the summary cursor table, …) before the actor's `run` loop
//!   exits, and re-hydratable when a fresh actor is spawned for the
//!   same `SessionId`. The struct derives `Serialize` / `Deserialize`
//!   so it is structurally provable that nothing non-roundtrippable
//!   leaks in.
//!
//! - [`VolatileResources`] holds everything else: shared infrastructure
//!   (Arc-backed registries, channels, cancellation tokens), per-actor
//!   derived caches (the [`AgentLoop`] itself with its in-memory
//!   transcript copy and `ContextManager` caches), and the supervisor
//!   handle the actor uses to self-deregister on shutdown.  None of it
//!   is persisted; it is rebuilt from the durable layer + the runtime
//!   [`crate::actor::router::ActorSpawner`] on every fresh actor.
//!
//! The sealed [`marker::Volatile`] trait enforces the boundary: adding
//! a new field to [`VolatileResources`] forces a compile-time decision
//! about its lifetime class.

use std::sync::Arc;

use baybo_channels::AgentOutput;
use baybo_model::Session;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::actor::supervisor::AgentSupervisor;
use crate::runtime::agent_loop::AgentLoop;
use baybo_job::JobLifecycle;
use baybo_session::SessionManager;
use baybo_trace::SpanRecorder;

pub mod marker;

use marker::Volatile;

/// Persistent half of [`crate::actor::AgentActor`]'s state.
///
/// Today this is a thin wrapper over [`Session`] (which already owns
/// transcript pointers, summary cursors, etc.). The wrapper exists so:
///
/// 1. Future fields that live exclusively in actor memory but should
///    survive eviction have a typed home — adding them here forces the
///    author to provide hydration + persistence.
/// 2. The `Serialize + DeserializeOwned` requirement on the durable
///    half is enforced by the derive below and the compile-time
///    assertion at the bottom of this file.
/// 3. Tests can snapshot the durable half and round-trip it through a
///    serializer to prove no `#[serde(skip)]` field is silently lost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableActorState {
    pub session: Session,
}

impl DurableActorState {
    pub fn new(session: Session) -> Self {
        Self { session }
    }
}

/// Volatile half of [`crate::actor::AgentActor`]'s state.
///
/// Every field is either a **shared resource** (cheap to clone, lives
/// for the duration of the process), a **per-actor handle** (rebuilt by
/// the spawner factory each time), or a **derived cache** ([`AgentLoop`]
/// — owns the context manager, billed-chat factory, and other state
/// rebuildable from [`DurableActorState`] + the session store).
///
/// Mark-checked by [`crate::actor::state::marker`]: every field type must
/// implement the sealed `Volatile` trait, so adding a value type (e.g.
/// a `String` counter) here fails to compile and forces the author
/// into [`DurableActorState`] instead.
pub struct VolatileResources {
    pub agent_loop: AgentLoop,
    pub response_tx: mpsc::Sender<AgentOutput>,
    pub job_lifecycle: Arc<JobLifecycle>,
    pub span_recorder: Arc<SpanRecorder>,
    /// Lifetime token for this actor. Derived as a child of the
    /// `parent_token` passed at construction. For top-level user/cron
    /// sessions the parent is the process-wide actor parent token; for
    /// subagent children it is the originating parent actor's
    /// `actor_token`, so parent shutdown cascades automatically via the
    /// `tokio_util` token tree.
    pub actor_token: CancellationToken,
    /// Supervisor handle used by [`crate::actor::supervisor::ActorRegistryGuard`]
    /// in [`crate::actor::AgentActor::run`] to self-deregister when the
    /// task exits. `None` for one-shot actors that are not tracked by
    /// the supervisor (cron fires, maintenance spawns, tests).
    pub supervisor: Option<AgentSupervisor>,
    /// Session-row writer used by handlers whose state mutations must
    /// survive actor eviction. Today only the background-subagent
    /// path reaches for this — `AgentMessage::BackgroundJobFinished`
    /// persists `session.state.pending_background_results`, and the
    /// next-turn drain re-persists the cleared list, so a parent that
    /// the idle reaper eventually reclaims still hands the pending
    /// notifications to the fresh actor on hydration.
    pub session_manager: Arc<SessionManager>,
    /// CRUD + transitions over this session's autonomous goal. Drives the
    /// turn-boundary continuation loop and the `/goal` command. Shares the
    /// `session_goals` table with the goal tools (see `baybo-goal`).
    pub goal_service: Arc<baybo_goal::GoalService>,
    /// Read side of the per-session token meter: the goal continuation loop
    /// samples `cost_manager.session_tokens(session_id)` before/after each goal
    /// turn and accrues the delta into the goal's `tokens_used`.
    pub cost_manager: Arc<baybo_cost::CostManager>,
}

/// Compile-time assertion: every field type of [`VolatileResources`]
/// implements [`Volatile`]. A field added without a corresponding
/// `Volatile` impl fails to compile here, forcing an audit: either
/// declare the type volatile-safe (in `state/marker.rs`) or move the
/// field into [`DurableActorState`] where it gets persisted. Updating
/// the list here is mandatory when the struct grows — rustc cannot
/// reflect over fields, so the audit point is manual.
const _ASSERT_VOLATILE_FIELDS: fn() = || {
    fn assert_volatile<T: Volatile>() {}
    assert_volatile::<AgentLoop>();
    assert_volatile::<mpsc::Sender<AgentOutput>>();
    assert_volatile::<Arc<JobLifecycle>>();
    assert_volatile::<Arc<SpanRecorder>>();
    assert_volatile::<CancellationToken>();
    assert_volatile::<Option<AgentSupervisor>>();
    assert_volatile::<Arc<SessionManager>>();
    assert_volatile::<Arc<baybo_goal::GoalService>>();
    assert_volatile::<Arc<baybo_cost::CostManager>>();
};

/// Compile-time assertion: [`DurableActorState`] is fully
/// serializable. Any field added that lacks `Serialize` /
/// `DeserializeOwned` will fail to compile here.
const _ASSERT_DURABLE_ROUNDTRIPS: fn() = || {
    fn assert_serde<T: Serialize + serde::de::DeserializeOwned>() {}
    assert_serde::<DurableActorState>();
};
