//! In-memory fakes for the store traits (feature `test-support`).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::AgentProfileId;
use parking_lot::Mutex;

use crate::agent_profile::{AgentProfileRow, AgentProfileStore, AgentProfileUpdate, Result};

/// In-memory [`AgentProfileStore`] for tests. No builtin seeding, no
/// name-uniqueness enforcement — insert exactly the rows the test needs.
#[derive(Default)]
pub struct MemoryAgentProfileStore {
    rows: Mutex<HashMap<AgentProfileId, AgentProfileRow>>,
}

impl MemoryAgentProfileStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn insert(&self, row: AgentProfileRow) {
        self.rows.lock().insert(row.id.clone(), row);
    }
}

#[async_trait]
impl AgentProfileStore for MemoryAgentProfileStore {
    async fn list(&self) -> Result<Vec<AgentProfileRow>> {
        Ok(self.rows.lock().values().cloned().collect())
    }
    async fn get(&self, id: &AgentProfileId) -> Result<Option<AgentProfileRow>> {
        Ok(self.rows.lock().get(id).cloned())
    }
    async fn create(&self, row: &AgentProfileRow) -> Result<()> {
        self.rows.lock().insert(row.id.clone(), row.clone());
        Ok(())
    }
    async fn update(&self, id: &AgentProfileId, update: &AgentProfileUpdate) -> Result<bool> {
        let mut rows = self.rows.lock();
        let Some(row) = rows.get_mut(id) else {
            return Ok(false);
        };
        row.name = update.name.clone();
        row.description = update.description.clone();
        row.system_prompt = update.system_prompt.clone();
        row.framework = update.framework;
        row.llm = update.llm.clone();
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
        Ok(self.rows.lock().remove(id).is_some())
    }
}
