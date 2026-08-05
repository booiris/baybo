//! In-memory store fakes for tests in this crate and downstream ones.
//!
//! Gated behind `cfg(test)` here and the `test-support` feature for
//! consumers, so nothing in this module ships in a release build.

use std::collections::HashMap;

use async_trait::async_trait;
use baybo_model::{AgentProfileId, LlmEntryName};
use parking_lot::Mutex;

use crate::agent_profile::{AgentProfileRow, AgentProfileStore, AgentProfileUpdate, Result};

/// A trivial [`AgentProfileStore`] over a `HashMap`, for tests that need the
/// runtime to resolve a bound agent without standing up sqlite.
///
/// Deliberately not a faithful reimplementation: it does not enforce the
/// builtin lock or name uniqueness (both are storage-layer concerns covered
/// by the sqlite tests). It answers `get` and `list`, which is what the
/// runtime consumers actually call.
#[derive(Default)]
pub struct MemoryAgentProfileStore {
    rows: Mutex<HashMap<AgentProfileId, AgentProfileRow>>,
}

impl MemoryAgentProfileStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed one row, replacing any existing row with the same id.
    pub fn insert(&self, row: AgentProfileRow) {
        self.rows.lock().insert(row.id.clone(), row);
    }

    /// Drop a row, so a test can exercise the deleted-profile fallback.
    pub fn remove(&self, id: &AgentProfileId) {
        self.rows.lock().remove(id);
    }
}

#[async_trait]
impl AgentProfileStore for MemoryAgentProfileStore {
    async fn list(&self) -> Result<Vec<AgentProfileRow>> {
        let mut rows: Vec<AgentProfileRow> = self
            .rows
            .lock()
            .values()
            .filter(|row| row.team.is_none())
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(rows)
    }

    async fn list_team(&self, project: &baybo_model::ProjectId) -> Result<Vec<AgentProfileRow>> {
        let mut rows: Vec<AgentProfileRow> = self
            .rows
            .lock()
            .values()
            .filter(|row| {
                row.deleted_at.is_none()
                    && row.team.as_ref().is_some_and(|t| &t.project_id == project)
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            a.team
                .as_ref()
                .map(|t| &t.handle)
                .cmp(&b.team.as_ref().map(|t| &t.handle))
        });
        Ok(rows)
    }

    async fn get(&self, id: &AgentProfileId) -> Result<Option<AgentProfileRow>> {
        Ok(self.rows.lock().get(id).cloned())
    }

    async fn create(&self, row: &AgentProfileRow) -> Result<()> {
        self.insert(row.clone());
        Ok(())
    }

    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool> {
        let mut rows = self.rows.lock();
        let Some(row) = rows.get_mut(id) else {
            return Ok(false);
        };
        row.description = update.description.clone();
        row.framework = update.framework;
        Ok(true)
    }

    async fn set_llm(&self, id: &AgentProfileId, llm: Option<&LlmEntryName>) -> Result<bool> {
        let mut rows = self.rows.lock();
        let Some(row) = rows.get_mut(id) else {
            return Ok(false);
        };
        row.llm = llm.cloned();
        Ok(true)
    }

    async fn set_avatar(&self, id: &AgentProfileId, blob_id: Option<&str>) -> Result<bool> {
        let mut rows = self.rows.lock();
        let Some(row) = rows.get_mut(id) else {
            return Ok(false);
        };
        row.avatar_blob_id = blob_id.map(str::to_owned);
        Ok(true)
    }

    async fn delete(&self, id: &AgentProfileId) -> Result<bool> {
        let mut rows = self.rows.lock();
        if rows.get(id).is_some_and(|row| row.team.is_some()) {
            return Ok(false);
        }
        Ok(rows.remove(id).is_some())
    }

    async fn remove_from_team(&self, id: &AgentProfileId) -> Result<bool> {
        let mut rows = self.rows.lock();
        let Some(row) = rows.get_mut(id) else {
            return Ok(false);
        };
        if row.team.is_none() || row.deleted_at.is_some() {
            return Ok(false);
        }
        row.deleted_at = Some(chrono::Utc::now());
        Ok(true)
    }
}

/// A profile row with everything defaulted except its id — the fields a
/// runtime test cares about. The display name is not here: it lives in the
/// agent's own `IDENTITY.md`.
pub fn agent_profile_row(id: &AgentProfileId) -> AgentProfileRow {
    let now = chrono::Utc::now();
    AgentProfileRow {
        id: id.clone(),
        description: String::new(),
        avatar_blob_id: None,
        framework: baybo_model::AgentFramework::Baybo,
        llm: None,
        builtin: false,
        team: None,
        hired_by: None,
        deleted_at: None,
        created_at: now,
        updated_at: now,
    }
}
