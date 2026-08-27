//! Persistence interface for user-managed agent profiles (chat personas).
//!
//! One row per profile in `agent_profiles`, including the seeded built-in
//! `baybo` row (`builtin = 1`) that represents default behavior and is
//! read-only except its avatar. The builtin lock is structural: `update`
//! and `delete` execute with `WHERE builtin = 0`, and `create` never binds
//! the `builtin` column, so the seed is the only writer of `builtin = 1`.
//! See `docs/modules/agent-profiles.md`.

use async_trait::async_trait;
use baybo_model::{AgentFramework, AgentProfileId, LlmPin, ProjectId, TeamMembership};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// One row of `agent_profiles`.
///
/// `None` on a nullable field consistently means "inherit the default":
/// an unset [`LlmPin`] level → `default-llm`, the entry's own model, the
/// entry's own effort. Skills are not a profile field — they are read
/// live from the skill registry (see `docs/modules/agent-profiles.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfileRow {
    pub id: AgentProfileId,
    pub description: String,
    /// Full blob id (`sha256:<digest>.<read-token>`) from the blob store.
    pub avatar_blob_id: Option<String>,
    pub framework: AgentFramework,
    /// What this agent runs on. Three columns, one value: see [`LlmPin`]
    /// for why a writer may not set them apart.
    pub llm: LlmPin,
    /// Read-side state only — never bound on insert; the sqlite seed is
    /// the sole writer of `builtin = 1`.
    pub builtin: bool,
    /// Which project team this agent belongs to, if any. `None` is a
    /// global agent — the chat personas that predate projects, and the
    /// only rows [`AgentProfileStore::list`] returns.
    pub team: Option<TeamMembership>,
    /// Which agent hired this one. `None` means the operator created it:
    /// there is no third possibility, so this is a nullable id rather than
    /// a second `User | Agent` enum next to [`crate::project::IssueActor`].
    pub hired_by: Option<AgentProfileId>,
    /// When this agent was removed from its team. The row survives:
    /// `issues.assignee`, `issue_runs.agent_id` and every timeline entry
    /// name it, and a board that cannot say who did the work is worse than
    /// one listing an agent nobody can assign. Global agents are still
    /// deleted outright — nothing references them by id.
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Full content state for a full-replace update: everything a `PUT` may
/// change — no `id`, no `avatar_blob_id` (owned by
/// [`AgentProfileStore::set_avatar`]), no `builtin`, no timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfileUpdate {
    pub description: String,
    pub framework: AgentFramework,
}

/// Agent-profile lifecycle persistence. The store is a dumb writer — the
/// llm/avatar-blob checks live one layer up (the gateway handlers); the only
/// policy baked in here is the structural builtin lock.
///
/// There is no display name in this trait: an agent's name lives in its own
/// `IDENTITY.md`, so it is neither stored nor unique. Rows order by id and
/// the gateway sorts by the derived name after reading it.
#[async_trait]
pub trait AgentProfileStore: Send + Sync {
    async fn list(&self) -> Result<Vec<AgentProfileRow>>;

    async fn list_team(&self, project: &ProjectId) -> Result<Vec<AgentProfileRow>>;

    /// Every agent that has ever been on this board, tombstones included,
    /// oldest first. The live roster filters removals; a history read must
    /// not, because the board's activity feed says when each teammate
    /// joined — and an entry that vanishes the day somebody leaves is the
    /// record rewriting itself, which is the one thing the tombstone
    /// exists to prevent.
    async fn list_team_history(&self, project: &ProjectId) -> Result<Vec<AgentProfileRow>>;

    /// Fetch a single profile, or `None` if it doesn't exist.
    ///
    /// Reaches removed team members on purpose — that is what lets a
    /// timeline entry resolve the agent it names long after the agent left.
    async fn get(&self, id: &AgentProfileId) -> Result<Option<AgentProfileRow>>;

    /// Insert a new profile row. Never binds `builtin` (the schema default
    /// fills it with 0). A duplicate id is
    /// [`StorageError::Conflict`].
    async fn create(&self, row: &AgentProfileRow) -> Result<()>;

    /// Full-replace the content fields and bump `updated_at`.
    ///
    /// Reaches the builtin, with one carve-out: **its framework is never
    /// written**. The built-in agent runs on baybo by definition — that is
    /// what makes its row the default behaviour — so the statement leaves
    /// that column alone for `builtin = 1` rather than trusting callers.
    /// Its description is ordinary editable text. Returns `Ok(false)` only
    /// when no row matched.
    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool>;

    /// Set or clear the avatar and bump `updated_at`. Deliberately not
    /// builtin-guarded: the avatar is the builtin's, and picking a face for
    /// it says nothing about what "default behaviour" means. Returns
    /// `Ok(false)` if no row matched.
    async fn set_avatar(&self, id: &AgentProfileId, blob_id: Option<&str>) -> Result<bool>;

    /// Set the avatar only while the row is still faceless. This is the
    /// compare-and-set door for generated defaults racing an operator's
    /// explicit choice. Returns `Ok(false)` when the row is missing or already
    /// has an avatar.
    async fn set_avatar_if_empty(&self, id: &AgentProfileId, blob_id: &str) -> Result<bool>;

    /// Replace the LLM pin whole and bump `updated_at`.
    ///
    /// Whole, not per-level: the entry, the model within it and the thinking
    /// rung are one choice ([`LlmPin`]), and a setter that could write the
    /// model without the entry is how a board ends up naming a model its run
    /// is not using. [`LlmPin::unpinned`] clears it.
    ///
    /// **The builtin's pin is forced to `NULL` at every level**, whatever is
    /// passed: that row *is* default behaviour, so it follows `default-llm`
    /// by definition. Pinning it would duplicate that setting into a second
    /// place they could disagree — change `default-llm` instead. Like the
    /// framework pin, the statement enforces this rather than a caller
    /// remembering to, and it normalises a row an earlier build let drift.
    ///
    /// Returns `Ok(false)` if no row matched.
    async fn set_llm(&self, id: &AgentProfileId, pin: &LlmPin) -> Result<bool>;

    async fn delete(&self, id: &AgentProfileId) -> Result<bool>;

    async fn remove_from_team(&self, id: &AgentProfileId) -> Result<bool>;
}
