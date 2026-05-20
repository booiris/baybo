//! Store ports: the persistence-trait contracts (`*Store`) and the shared
//! [`StorageError`], decoupled from any concrete backend.
//!
//! Domain crates and other consumers depend on this crate to *call* a
//! store; `aura-storage` is the libsql adapter that *implements* the
//! traits. Keeping the contracts here — a leaf over `aura-model` — lets
//! low-level crates depend on a store interface without pulling the heavy
//! libsql adapter, and keeps the dependency graph acyclic.

pub mod cost;
pub mod cron;
pub mod error;
pub mod memory;
pub mod secret;
pub mod session;
pub mod session_summary;

pub use cost::CostStore;
pub use cron::CronStore;
pub use error::StorageError;
pub use memory::MemoryStore;
pub use secret::SecretStore;
pub use session::{SessionStore, StoredMessage};
pub use session_summary::{SessionSummaryRow, SessionSummaryStore};
