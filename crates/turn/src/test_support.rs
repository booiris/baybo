//! In-memory `TurnStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `baybo-turn` (next to the row conversions it
//! pairs with) so crates that depend on `baybo-turn` can spin up a fake
//! store without pulling the sqlite adapter.

use std::collections::HashMap;

use async_trait::async_trait;
use baybo_model::{SessionId, TurnId};
use baybo_store::turn::Result;
use baybo_store::{SessionTurnStats, TurnRow, TurnStore};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

/// In-memory `TurnStore` for tests. Keyed by `row.id`.
#[derive(Debug, Default)]
pub struct MemoryTurnStore {
    turns: Mutex<HashMap<TurnId, TurnRow>>,
}

impl MemoryTurnStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.turns.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl TurnStore for MemoryTurnStore {
    async fn create(&self, turn: &TurnRow) -> Result<()> {
        self.turns.lock().insert(turn.id, turn.clone());
        Ok(())
    }

    async fn get(&self, turn_id: &TurnId) -> Result<Option<TurnRow>> {
        Ok(self.turns.lock().get(turn_id).cloned())
    }

    async fn save(&self, turn: &TurnRow) -> Result<()> {
        self.turns.lock().insert(turn.id, turn.clone());
        Ok(())
    }

    async fn list_by_session(&self, session_id: &SessionId) -> Result<Vec<TurnRow>> {
        Ok(self
            .turns
            .lock()
            .values()
            .filter(|j| &j.session_id == session_id)
            .cloned()
            .collect())
    }

    async fn list_active_by_session(&self, session_id: &SessionId) -> Result<Vec<TurnRow>> {
        Ok(self
            .turns
            .lock()
            .values()
            .filter(|j| {
                &j.session_id == session_id
                    && matches!(j.status_kind.as_str(), "pending" | "in_progress" | "stuck")
            })
            .cloned()
            .collect())
    }

    async fn list_by_status_kind(&self, status_kind: &str) -> Result<Vec<TurnRow>> {
        Ok(self
            .turns
            .lock()
            .values()
            .filter(|j| j.status_kind == status_kind)
            .cloned()
            .collect())
    }

    async fn list_recoverable(&self) -> Result<Vec<TurnRow>> {
        Ok(self
            .turns
            .lock()
            .values()
            .filter(|j| matches!(j.status_kind.as_str(), "pending" | "in_progress" | "stuck"))
            .cloned()
            .collect())
    }

    async fn list_children(&self, parent_turn_id: &TurnId) -> Result<Vec<TurnRow>> {
        Ok(self
            .turns
            .lock()
            .values()
            .filter(|j| j.parent_turn_id.as_ref() == Some(parent_turn_id))
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<TurnRow>> {
        Ok(self.turns.lock().values().cloned().collect())
    }

    async fn list_page(
        &self,
        status_kind: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<TurnRow>, usize)> {
        let mut rows: Vec<TurnRow> = self
            .turns
            .lock()
            .values()
            .filter(|j| status_kind.is_none_or(|k| j.status_kind == k))
            .cloned()
            .collect();
        rows.sort_by_key(|j| std::cmp::Reverse(j.created_at));
        let total = rows.len();
        let page = rows.into_iter().skip(offset).take(limit).collect();
        Ok((page, total))
    }

    async fn count_by_status_kind(&self, status_kind: &str) -> Result<usize> {
        Ok(self
            .turns
            .lock()
            .values()
            .filter(|j| j.status_kind == status_kind)
            .count())
    }

    async fn session_turn_stats(&self) -> Result<Vec<SessionTurnStats>> {
        let mut by_session: HashMap<SessionId, (usize, DateTime<Utc>, String)> = HashMap::new();
        for j in self.turns.lock().values() {
            let entry = by_session.entry(j.session_id.clone()).or_insert((
                0,
                j.created_at,
                j.status_kind.clone(),
            ));
            entry.0 += 1;
            if j.created_at >= entry.1 {
                entry.1 = j.created_at;
                entry.2 = j.status_kind.clone();
            }
        }
        Ok(by_session
            .into_iter()
            .map(
                |(session_id, (turn_count, _, latest_status_kind))| SessionTurnStats {
                    session_id,
                    turn_count,
                    latest_status_kind,
                },
            )
            .collect())
    }
}
