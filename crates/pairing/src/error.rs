use thiserror::Error;

/// Error surface for the iOS-companion device-pairing flow. Unlike
/// [`PairingError`] (whose store returns `String`), the device stores return a
/// typed [`baybo_store::StorageError`], so it threads through directly.
#[derive(Debug, Error)]
pub enum DevicePairingError {
    #[error("storage: {0}")]
    Storage(#[from] baybo_store::StorageError),

    #[error("code: {0}")]
    Code(#[from] crate::code::CodeError),

    /// The single-use pairing slot was already consumed — a concurrent
    /// handshake for the same code won the race.
    #[error("pairing slot already consumed")]
    SlotConsumed,
}

#[derive(Debug, Error)]
pub enum PairingError {
    /// Any failure from the persistence layer. Carried as a string so
    /// the store trait doesn't have to leak its error type up through
    /// the service.
    #[error("storage: {0}")]
    Storage(String),

    /// The code generator couldn't find an unused code inside the
    /// configured retry budget. Treated as an internal error —
    /// practically unreachable on a healthy pending queue.
    #[error("code: {0}")]
    Code(#[from] crate::code::CodeError),
}
