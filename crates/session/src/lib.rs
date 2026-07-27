//! Session orchestration + persistence interface.
//!
//! Domain types (`Session`, `User`, `ChannelType`, `SessionState`,
//! `Lineage`, `TriggerSource`) live in `baybo-model`; the `SessionStore`
//! trait and its per-row `StoredMessage` value type live in
//! `baybo-store` (the ports
//! crate). This crate owns the `SessionManager` business-logic facade;
//! `baybo-storage` provides the sqlite implementations of both stores.

mod error;
mod manager;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use baybo_store::{
    SessionFolderRow, SessionFolderStore, SessionMessageAppendOutcome, SessionStore, StoredMessage,
};
pub use error::SessionError;
pub use manager::SessionManager;
pub type Result<T> = std::result::Result<T, SessionError>;
