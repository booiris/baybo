//! Persistence interface for the `human_asks` ledger: every prompt the agent
//! handed to a human, written before it is shown and kept forever.
//!
//! A row moves `pending` (`resolved_at` unset) → `resolved` (a human or a
//! policy decided; `applied_at` unset) → `applied` (a turn consumed the
//! decision). Both transitions are claims: exactly one caller wins. There is
//! no delete — resolved rows are the audit trail. See
//! `docs/modules/human-ask.md`.

use async_trait::async_trait;
use baybo_model::human_ask::{AskId, AskKind, AskPhase, AskRequest, AskResolution};
use baybo_model::{ChannelType, SessionId};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// One ask as the ledger stores it.
#[derive(Debug, Clone, PartialEq)]
pub struct HumanAskRow {
    pub ask_id: AskId,
    pub session_id: SessionId,
    /// Which channel's queue a pending row is reloaded into after a restart.
    pub channel: ChannelType,
    /// LLM `tool_use_id` of the call the ask blocks. Not unique per row.
    pub tool_call_id: String,
    pub kind: AskKind,
    pub phase: AskPhase,
    pub request: AskRequest,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolution: Option<AskResolution>,
    pub applied_at: Option<DateTime<Utc>>,
}

impl HumanAskRow {
    /// `true` while nobody has decided the ask.
    pub fn is_pending(&self) -> bool {
        self.resolved_at.is_none()
    }
}

#[async_trait]
pub trait HumanAskStore: Send + Sync {
    /// Insert a row. Idempotent on `ask_id`: a second insert with the same id
    /// is a no-op and leaves the first row untouched.
    async fn create(&self, row: &HumanAskRow) -> Result<()>;

    /// Claim the pending → resolved transition. `Ok(true)` for the one caller
    /// that won; `Ok(false)` when the ask is already resolved or absent.
    async fn resolve(
        &self,
        ask_id: &AskId,
        resolution: &AskResolution,
        now: DateTime<Utc>,
    ) -> Result<bool>;

    /// Claim the stamp that a turn has consumed the ask. `Ok(true)` for the
    /// one caller that won; `Ok(false)` when already applied or absent.
    async fn mark_applied(&self, ask_id: &AskId, now: DateTime<Utc>) -> Result<bool>;

    async fn get(&self, ask_id: &AskId) -> Result<Option<HumanAskRow>>;

    /// Every row not yet applied (pending or resolved), oldest first. A row
    /// whose stored kind, phase or payload this build does not recognise is
    /// skipped rather than failing the call.
    async fn list_unapplied(&self) -> Result<Vec<HumanAskRow>>;

    /// [`Self::list_unapplied`], scoped to one session.
    async fn list_unapplied_for_session(&self, session_id: &SessionId) -> Result<Vec<HumanAskRow>>;
}
