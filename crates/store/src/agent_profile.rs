//! Persistence interface for user-managed agent profiles (chat personas).
//!
//! One row per profile in `agent_profiles`, including the seeded built-in
//! `baybo` row (`builtin = 1`) that represents default behavior and is
//! read-only except its avatar. The builtin lock is structural: `update`
//! and `delete` execute with `WHERE builtin = 0`, and `create` never binds
//! the `builtin` column, so the seed is the only writer of `builtin = 1`.
//! See `docs/modules/agent-profiles.md`.

use async_trait::async_trait;
use baybo_model::{AgentFramework, AgentProfileId, LlmEntryName, ProjectId, TeamMembership};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// One row of `agent_profiles`.
///
/// `None` on a nullable field consistently means "inherit the default":
/// `llm` → `default-llm`. Skills are not a profile field — they are read
/// live from the skill registry (see `docs/modules/agent-profiles.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfileRow {
    pub id: AgentProfileId,
    pub description: String,
    /// Full blob id (`sha256:<digest>.<read-token>`) from the blob store.
    pub avatar_blob_id: Option<String>,
    pub framework: AgentFramework,
    pub llm: Option<LlmEntryName>,
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
    /// Every **global** profile, builtin first then by id.
    ///
    /// Project agents are excluded by the statement, not by the caller: this
    /// list feeds the Agents page, the session-binding picker and the
    /// subagent roster, none of which should offer somebody else's teammate.
    /// Use [`Self::list_team`] to read a board's roster.
    async fn list(&self) -> Result<Vec<AgentProfileRow>>;

    /// One project's live team, by handle.
    async fn list_team(&self, project: &ProjectId) -> Result<Vec<AgentProfileRow>>;

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

    /// Set or clear the LLM pin and bump `updated_at`.
    ///
    /// **The builtin's pin is forced to `NULL`**, whatever is passed: that
    /// row *is* default behaviour, so it follows `default-llm` by
    /// definition. Pinning it would duplicate that setting into a second
    /// place they could disagree — change `default-llm` instead. Like the
    /// framework pin, the statement enforces this rather than a caller
    /// remembering to, and it normalises a row an earlier build let drift.
    ///
    /// Returns `Ok(false)` if no row matched.
    async fn set_llm(&self, id: &AgentProfileId, llm: Option<&LlmEntryName>) -> Result<bool>;

    /// Plain row delete, guarded `WHERE builtin = 0 AND project_id IS NULL`.
    /// Returns `Ok(false)` if no row matched (missing id, the builtin, or a
    /// team member — those leave through [`Self::remove_from_team`] and
    /// keep their row).
    async fn delete(&self, id: &AgentProfileId) -> Result<bool>;

    /// Tombstone a team member: stamp `deleted_at` so the roster stops
    /// offering it while every issue, run and timeline entry that names it
    /// still resolves. Returns `Ok(false)` if no live team member matched.
    async fn remove_from_team(&self, id: &AgentProfileId) -> Result<bool>;
}
