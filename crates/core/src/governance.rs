use serde::{Deserialize, Serialize};

/// Trust level assigned to an extension (skill or tool).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrustLevel {
    /// Placed in the user workspace or by an administrator.
    Trusted,
    /// Installed through the registry.
    Installed,
    /// May only be listed and reviewed; cannot auto-execute.
    Untrusted,
}

/// Where an extension artifact originates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactSource {
    /// From the local workspace directory.
    Workspace,
    /// From a remote registry.
    Registry { url: String },
    /// From a local file path.
    Local { path: String },
}
