use serde::{Deserialize, Serialize};

/// Mirror of `aura_model::TrustLevel`. The consumer maps between them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevelConfig {
    Trusted,
    Installed,
    Untrusted,
}
