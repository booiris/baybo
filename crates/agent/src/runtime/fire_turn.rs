//! What a recurring cron fire's turns share with the agent loop.

use std::sync::Arc;

use baybo_model::BackgroundHandle;

/// Handed to the agent loop by a recurring cron fire's own turn and by its
/// report's turn — and by no other. The actor owns it, so what the turn
/// records stays readable even when the turn itself fails.
#[derive(Clone, Default)]
pub struct FireTurn {
    /// Flipped by `report_nothing` to silence the turn.
    pub silence: baybo_tools::NotifySilence,
    /// Every job the turn hands to the background, recorded as it happens.
    handed_over: Arc<parking_lot::Mutex<Vec<BackgroundHandle>>>,
}

impl FireTurn {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_hand_over(&self, handle: BackgroundHandle) {
        self.handed_over.lock().push(handle);
    }

    /// The jobs the turn handed to the background, in order.
    pub fn handed_over(&self) -> Vec<BackgroundHandle> {
        self.handed_over.lock().clone()
    }
}
