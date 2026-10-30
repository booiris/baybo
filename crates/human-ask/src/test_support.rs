//! In-memory [`HumanAskStore`] for tests that need a ledger without sqlite.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use baybo_model::SessionId;
use baybo_model::human_ask::{AskId, AskResolution};
use baybo_store::{HumanAskRow, HumanAskStore, StorageError};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

/// Same claim semantics as the sqlite store: `create` is insert-or-ignore,
/// `resolve` / `mark_applied` win once.
#[derive(Default)]
pub struct MemoryHumanAskStore {
    rows: Mutex<Vec<HumanAskRow>>,
    fail_create: AtomicBool,
}

impl MemoryHumanAskStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Make every later `create` fail, as a full disk would.
    pub fn fail_creates(&self) {
        self.fail_create.store(true, Ordering::SeqCst);
    }

    /// Every row, in insertion order.
    pub fn rows(&self) -> Vec<HumanAskRow> {
        self.rows.lock().clone()
    }

    pub fn row(&self, ask_id: &AskId) -> Option<HumanAskRow> {
        self.rows
            .lock()
            .iter()
            .find(|r| &r.ask_id == ask_id)
            .cloned()
    }

    /// Seed a row as-is, bypassing `create`'s failure switch.
    pub fn insert(&self, row: HumanAskRow) {
        self.rows.lock().push(row);
    }
}

#[async_trait]
impl HumanAskStore for MemoryHumanAskStore {
    async fn create(&self, row: &HumanAskRow) -> baybo_store::human_ask::Result<()> {
        if self.fail_create.load(Ordering::SeqCst) {
            return Err(StorageError::Storage("create disabled by test".into()));
        }
        let mut rows = self.rows.lock();
        if !rows.iter().any(|r| r.ask_id == row.ask_id) {
            rows.push(row.clone());
        }
        Ok(())
    }

    async fn resolve(
        &self,
        ask_id: &AskId,
        resolution: &AskResolution,
        now: DateTime<Utc>,
    ) -> baybo_store::human_ask::Result<bool> {
        let mut rows = self.rows.lock();
        match rows
            .iter_mut()
            .find(|r| &r.ask_id == ask_id && r.resolved_at.is_none())
        {
            Some(row) => {
                row.resolved_at = Some(now);
                row.resolution = Some(resolution.clone());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn mark_applied(
        &self,
        ask_id: &AskId,
        now: DateTime<Utc>,
    ) -> baybo_store::human_ask::Result<bool> {
        let mut rows = self.rows.lock();
        match rows
            .iter_mut()
            .find(|r| &r.ask_id == ask_id && r.applied_at.is_none())
        {
            Some(row) => {
                row.applied_at = Some(now);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn get(&self, ask_id: &AskId) -> baybo_store::human_ask::Result<Option<HumanAskRow>> {
        Ok(self.row(ask_id))
    }

    async fn list_unapplied(&self) -> baybo_store::human_ask::Result<Vec<HumanAskRow>> {
        Ok(self
            .rows
            .lock()
            .iter()
            .filter(|r| r.applied_at.is_none())
            .cloned()
            .collect())
    }

    async fn list_unapplied_for_session(
        &self,
        session_id: &SessionId,
    ) -> baybo_store::human_ask::Result<Vec<HumanAskRow>> {
        Ok(self
            .rows
            .lock()
            .iter()
            .filter(|r| r.applied_at.is_none() && &r.session_id == session_id)
            .cloned()
            .collect())
    }
}

/// A reaper probe that never sees a parked ask.
pub struct NoLivePendingAsks;

impl crate::ports::LivePendingAsks for NoLivePendingAsks {
    fn has_live_pending_ask(&self, _session_id: &SessionId) -> bool {
        false
    }
}

/// A resume sink for fixtures with no router: an answer to a dead ask
/// resumes nothing.
pub struct NoResume;

#[async_trait]
impl crate::ports::ResumeSink for NoResume {
    async fn resumes_on_answer(&self, _session_id: &SessionId) -> bool {
        false
    }

    fn request_resume(&self, _req: baybo_model::human_ask::ResumeRequest) {}
}
