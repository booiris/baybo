mod governance;

pub use governance::{ArtifactSource, TrustLevel};

use serde::{Deserialize, Serialize};

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
