//! The store error type now lives in the `aura-store` ports crate so the
//! trait contracts and their callers can share it without depending on
//! this libsql adapter. Re-exported here for the adapter's own use and
//! for back-compat with `aura_storage::StorageError` call sites.
pub use aura_store::StorageError;
