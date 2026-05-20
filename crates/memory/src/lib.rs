//! Long-term memory subsystem — domain types live in `aura-model`
//! (`MemoryEntry`, `MemoryCategory`). This crate owns the
//! `MemoryStore` trait and the `MemoryManager` business-logic facade
//! (list/search/store/delete/importance) used by the admin REST surface.
//! There is currently no automatic recall or auto-store path; the agent
//! loop does not consult this subsystem.
//!
//! `aura-storage` provides the libsql implementation of `MemoryStore`;
//! the trait itself lives here so downstream callers and tests can
//! depend on `aura-memory` alone for memory-management work.

mod error;
mod manager;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::MemoryError;
pub use manager::MemoryManager;

pub type Result<T> = std::result::Result<T, MemoryError>;
