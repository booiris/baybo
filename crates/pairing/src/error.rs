use thiserror::Error;

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
