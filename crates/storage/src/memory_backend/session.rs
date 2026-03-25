use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

use aura_core::{Result, Session};
use aura_session::SessionStore;

/// In-memory implementation of [`SessionStore`].
pub struct InMemorySessionStore {
    data: Arc<RwLock<HashMap<String, Session>>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self {
            data: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn get(&self, session_id: &str) -> Result<Option<Session>> {
        let guard = self.data.read().await;
        Ok(guard.get(session_id).cloned())
    }

    async fn save(&self, session: &Session) -> Result<()> {
        let mut guard = self.data.write().await;
        guard.insert(session.id.clone(), session.clone());
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<()> {
        let mut guard = self.data.write().await;
        guard.remove(session_id);
        Ok(())
    }

    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>> {
        let guard = self.data.read().await;
        let expired = guard
            .iter()
            .filter(|(_, s)| s.last_active < before)
            .map(|(id, _)| id.clone())
            .collect();
        Ok(expired)
    }
}
