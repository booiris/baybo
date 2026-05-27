//! # aura-security
//!
//! Low-level security primitives: AES-256-GCM crypto, deterministic
//! placeholder minting, leak detection, prompt-injection scanning, and
//! sensitive-path checks.
//!
//! Prompt-injection rules, sensitive-path patterns, and several leak
//! heuristics are adapted from NEAR AI's ironclaw project:
//! <https://github.com/nearai/ironclaw/tree/staging/crates/ironclaw_safety/src>.

pub mod crypto;
pub mod error;
pub mod http;
pub mod injection_detector;
pub mod leak_detector;
pub mod log_redact;
pub mod network;
pub mod placeholder;
pub mod secret_value;
pub mod secret_vault;
pub mod sensitive_paths;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::SecurityError;

pub type Result<T> = std::result::Result<T, SecurityError>;

// Re-exports for convenient access.
pub use crate::crypto::EncryptionKey;
pub use crate::http::ProxySettings;
pub use crate::injection_detector::{InjectionDetector, InjectionSeverity, InjectionWarning};
pub use crate::leak_detector::{
    LeakAction, LeakDetectionRule, LeakDetector, LeakMatch, LeakScanResult,
};
pub use crate::log_redact::{RedactingMakeWriter, RedactingWriter};
pub use crate::network::is_blocked_ip;
pub use crate::placeholder::PlaceholderMinter;
pub use crate::secret_value::SecretValue;
pub use crate::secret_vault::SecretVault;
pub use crate::sensitive_paths::is_sensitive_path;
