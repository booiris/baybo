pub mod crypto;
pub mod error;
pub mod leak_detector;

pub use error::SecurityError;

pub type Result<T> = std::result::Result<T, SecurityError>;

// Re-exports for convenient access.
pub use crate::crypto::EncryptionKey;
pub use crate::leak_detector::{LeakAction, LeakDetectionRule, LeakDetector};
