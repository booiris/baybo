//! Persistence interface for user-managed agent profiles (chat personas).
//!
//! One row per profile in `agent_profiles`, including the seeded built-in
//! `baybo` row (`builtin = 1`) that represents default behavior and is
//! read-only except its avatar. The builtin lock is structural: `update`
//! and `delete` execute with `WHERE builtin = 0`, and `create` never binds
//! the `builtin` column, so the seed is the only writer of `builtin = 1`.
//! See `docs/modules/agent-profiles.md`.

use async_trait::async_trait;
use baybo_model::{AgentFramework, AgentProfileId, LlmEntryName};
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
    pub llm: Option<LlmEntryName>,
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
    /// Every profile, builtin first then by id.
    async fn list(&self) -> Result<Vec<AgentProfileRow>>;

    /// Fetch a single profile, or `None` if it doesn't exist.
    async fn get(&self, id: &AgentProfileId) -> Result<Option<AgentProfileRow>>;

    /// Insert a new profile row. Never binds `builtin` (the schema default
    /// fills it with 0). A duplicate id is
    /// [`StorageError::Conflict`].
    async fn create(&self, row: &AgentProfileRow) -> Result<()>;

    /// Full-replace the content fields and bump `updated_at`. Guarded
    /// `WHERE builtin = 0`; returns `Ok(false)` if no row matched (missing
    /// id, or the builtin behind the guard).
    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool>;

    /// Set or clear the avatar and bump `updated_at`. Deliberately not
    /// builtin-guarded — the avatar is the one field the builtin allows.
    /// Returns `Ok(false)` if no row matched.
    async fn set_avatar(&self, id: &AgentProfileId, blob_id: Option<&str>) -> Result<bool>;

    /// Plain row delete, guarded `WHERE builtin = 0`. Returns `Ok(false)`
    /// if no row matched (missing id, or the builtin behind the guard).
    async fn delete(&self, id: &AgentProfileId) -> Result<bool>;
}
