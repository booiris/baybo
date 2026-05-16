//! Long-term memory subsystem — domain types live in `aura-model`
//! (`MemoryEntry`, `MemoryCategory`). This crate owns the
//! `MemoryStore` trait and the `MemoryManager` business-logic facade
//! (recall, store, dedup, importance, embedding hook).
//!
//! `aura-storage` provides the libsql implementation of `MemoryStore`;
//! the trait itself lives here so downstream callers and tests can
//! depend on `aura-memory` alone for memory-management work.

mod error;
mod manager;
mod store;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::MemoryError;
pub use manager::{EmbeddingModel, MemoryManager};
pub use store::MemoryStore;

pub type Result<T> = std::result::Result<T, MemoryError>;
