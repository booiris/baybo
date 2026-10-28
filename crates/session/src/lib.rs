//! Session orchestration + persistence interface.
//!
//! Domain types (`Session`, `User`, `ChannelType`, `SessionState`,
//! `Lineage`, `TriggerSource`) live in `baybo-model`; the `SessionStore`
//! trait and its `ActiveMessageRow` / `StoredMessage` row types live in
//! `baybo-store` (the ports
//! crate). This crate owns the `SessionManager` business-logic facade and
//! the grouped transcript search service `search::SessionSearch`;
//! `baybo-storage` provides the sqlite implementations of the stores.

mod error;
mod manager;
pub mod search;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use baybo_store::{
    ActiveMessageRow, NewTokenAnchor, SessionFolderRow, SessionFolderStore,
    SessionMessageAppendOutcome, SessionStore, StoredMessage, TokenAnchor,
};
pub use error::SessionError;
pub use manager::SessionManager;
pub type Result<T> = std::result::Result<T, SessionError>;
