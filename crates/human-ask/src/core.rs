//! [`AskCore`]: the ledger and its observers, shared by the gates and the
//! manager so a gate can persist its own ask without a back-edge to the
//! manager.
//!
//! The store is not handed out. Every write that changes what an ask says
//! goes through a verb here that tells the observers once it has landed, so
//! a new way to raise or close an ask cannot leave them behind.

use std::sync::Arc;

use baybo_model::SessionId;
use baybo_model::human_ask::{AskId, AskResolution};
use baybo_store::{HumanAskRow, HumanAskStore};
use chrono::Utc;
use parking_lot::RwLock;

use crate::error::Result;
use crate::ports::AskObserver;

/// What an [`AskObserver`] is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskEvent {
    /// The row was written and the ask is about to be shown.
    Raised,
    /// A verb claimed the row's resolution.
    Resolved(AskResolution),
}

pub struct AskCore {
    store: Arc<dyn HumanAskStore>,
    observers: RwLock<Vec<Arc<dyn AskObserver>>>,
}

impl AskCore {
    pub fn new(store: Arc<dyn HumanAskStore>) -> Self {
        Self {
            store,
            observers: RwLock::new(Vec::new()),
        }
    }

    /// Append an observer. Takes `&self` because it is wired after the gates
    /// already share the core.
    pub fn add_observer(&self, observer: Arc<dyn AskObserver>) {
        self.observers.write().push(observer);
    }

    pub(crate) async fn get(&self, ask_id: &AskId) -> Result<Option<HumanAskRow>> {
        Ok(self.store.get(ask_id).await?)
    }

    pub(crate) async fn list_unapplied(&self) -> Result<Vec<HumanAskRow>> {
        Ok(self.store.list_unapplied().await?)
    }

    pub(crate) async fn list_unapplied_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<HumanAskRow>> {
        Ok(self.store.list_unapplied_for_session(session_id).await?)
    }

    /// Write a pending row, then tell the observers it was raised.
    pub(crate) async fn raise(&self, row: &HumanAskRow) -> Result<()> {
        self.store.create(row).await?;
        self.notify(row, AskEvent::Raised).await;
        Ok(())
    }

    /// Write `row` already resolved to `resolution` and applied — an ask
    /// decided the moment it was raised, such as a policy denial — then tell
    /// the observers its resolution. No `Raised` precedes it: nothing was
    /// ever shown.
    pub(crate) async fn record_settled(
        &self,
        mut row: HumanAskRow,
        resolution: AskResolution,
    ) -> Result<()> {
        let now = Utc::now();
        row.resolved_at = Some(now);
        row.resolution = Some(resolution.clone());
        row.applied_at = Some(now);
        self.store.create(&row).await?;
        self.notify(&row, AskEvent::Resolved(resolution)).await;
        Ok(())
    }

    /// Claim `resolution` on a pending row. On a win, mirror it onto `row`
    /// and tell the observers; `Ok(false)` when another verb claimed first,
    /// and nobody is told.
    pub(crate) async fn claim(
        &self,
        row: &mut HumanAskRow,
        resolution: AskResolution,
    ) -> Result<bool> {
        let now = Utc::now();
        if !self.store.resolve(&row.ask_id, &resolution, now).await? {
            return Ok(false);
        }
        row.resolved_at = Some(now);
        row.resolution = Some(resolution.clone());
        self.notify(row, AskEvent::Resolved(resolution)).await;
        Ok(true)
    }

    async fn notify(&self, row: &HumanAskRow, event: AskEvent) {
        let observers = self.observers.read().clone();
        for observer in observers {
            observer.on_event(row, event.clone()).await;
        }
    }

    /// Claim the applied stamp. `Ok(false)` when it was already applied.
    pub(crate) async fn mark_applied(&self, ask_id: &AskId) -> Result<bool> {
        Ok(self.store.mark_applied(ask_id, Utc::now()).await?)
    }
}
