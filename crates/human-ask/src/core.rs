//! [`AskCore`]: the ledger and its observers, shared by the gates and the
//! manager so a gate can persist its own ask without a back-edge to the
//! manager.

use std::sync::Arc;

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

    pub(crate) fn store(&self) -> &dyn HumanAskStore {
        self.store.as_ref()
    }

    pub(crate) async fn notify(&self, row: &HumanAskRow, event: AskEvent) {
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
