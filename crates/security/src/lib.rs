pub mod crypto;
pub mod gateway;
pub mod leak_detector;
pub mod vault;

use aura_core::Result;

// Re-exports for convenient access.
pub use crate::crypto::EncryptionKey;
pub use crate::gateway::{
    DenyAllPolicy, NetworkPolicyDecider, NetworkPolicyDecision, NetworkRequest, SecurityGateway,
};
pub use crate::leak_detector::{LeakAction, LeakDetectionRule, LeakDetector};
pub use crate::vault::{SecretValue, SecretVault};

/// Async trait for encrypted secret persistence.
///
/// Implementations (in-memory, SQLite, etc.) live in the `storage` crate.
/// The `security` crate only defines the interface.
#[async_trait::async_trait]
pub trait SecretStore: Send + Sync {
    /// Persist an encrypted secret under the given name.
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()>;

    /// Retrieve the encrypted bytes for a secret, or `None` if not found.
    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>>;

    /// Delete a secret by name.
    async fn delete(&self, name: &str) -> Result<()>;

    /// List all stored secret names.
    async fn list(&self) -> Result<Vec<String>>;
}
