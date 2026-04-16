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

/// Where an extension artifact was sourced from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactSource {
    /// Placed directly in the workspace.
    Workspace,
    /// Downloaded from the extension registry.
    Registry { url: String },
    /// Provided inline or via configuration.
    Inline,
}

/// Manifest describing an extension artifact for installation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    pub name: String,
    pub version: String,
    pub artifact_hash: String,
    pub signature: Option<String>,
    pub source_url: String,
    pub kind: ExtensionKind,
    pub trust_level: TrustLevel,
}

/// The kind of extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtensionKind {
    Skill,
    Tool,
}
