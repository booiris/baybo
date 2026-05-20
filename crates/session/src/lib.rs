//! Session orchestration + persistence interface.
//!
//! Domain types (`Session`, `User`, `ChannelType`, `SessionState`,
//! `Lineage`, `TriggerSource`) live in `aura-model`; this crate owns
//! the `SessionStore` / `SessionSummaryStore` traits, the
//! `SessionManager` business-logic facade, and the per-row
//! `StoredMessage` / `SessionSummaryRow` value types. `aura-storage`
//! provides the libsql implementations of both stores.

mod error;
mod manager;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use aura_store::{SessionStore, SessionSummaryRow, SessionSummaryStore, StoredMessage};
pub use error::SessionError;
pub use manager::SessionManager;
pub type Result<T> = std::result::Result<T, SessionError>;
