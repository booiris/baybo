pub mod crypto;
pub mod error;
pub mod gateway;
pub mod leak_detector;
pub mod vault;

pub use error::SecurityError;

pub type Result<T> = std::result::Result<T, SecurityError>;

// Re-exports for convenient access.
pub use crate::crypto::EncryptionKey;
pub use crate::gateway::SecurityGateway;
pub use crate::leak_detector::{LeakAction, LeakDetectionRule, LeakDetector};
pub use crate::vault::{SecretValue, SecretVault};

/// Async trait for encrypted secret persistence.
///
/// Implementations (in-memory, SQLite, etc.) live in the `storage` crate.
/// The `security` crate only defines the interface.
#[async_trait::async_trait]
pub trait SecretStore: Send + Sync {
    /// Persist an encrypted secret under the given name.
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> crate::Result<()>;

    /// Retrieve the encrypted bytes for a secret, or `None` if not found.
    async fn retrieve(&self, name: &str) -> crate::Result<Option<Vec<u8>>>;

    /// Delete a secret by name.
    async fn delete(&self, name: &str) -> crate::Result<()>;

    /// List all stored secret names.
    async fn list(&self) -> crate::Result<Vec<String>>;
}
