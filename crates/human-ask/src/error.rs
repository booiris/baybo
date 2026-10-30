use baybo_model::human_ask::{AskId, AskKind};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HumanAskError {
    #[error("human-ask store error: {reason}")]
    Store { reason: String },

    #[error("ask {ask_id} not found")]
    NotFound { ask_id: AskId },

    /// The ask was already resolved — another verb won the claim.
    #[error("ask {ask_id} is already closed")]
    AskClosed { ask_id: AskId },

    /// The resolution does not fit the ask's kind (an approval decision for a
    /// question, or answers for an approval).
    #[error("ask {ask_id} is a {kind:?} ask; that resolution does not apply to it")]
    KindMismatch { ask_id: AskId, kind: AskKind },
}

impl From<baybo_store::StorageError> for HumanAskError {
    fn from(e: baybo_store::StorageError) -> Self {
        Self::Store {
            reason: e.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, HumanAskError>;
