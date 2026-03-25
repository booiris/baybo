pub mod catalog;
pub mod verifier;

use std::path::PathBuf;

use aura_core::{AuraError, TrustLevel};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use catalog::{Catalog, CatalogEntry};
pub use verifier::ArtifactVerifier;

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

/// An artifact that has been verified and installed locally.
#[derive(Debug, Clone)]
pub struct InstalledArtifact {
    pub name: String,
    pub version: String,
    pub path: PathBuf,
    pub artifact_hash: String,
}

/// Handles downloading, verifying, and installing extension artifacts.
pub struct RegistryInstaller {
    install_root: PathBuf,
}

impl RegistryInstaller {
    pub fn new(install_root: PathBuf) -> Self {
        Self { install_root }
    }

    /// Verifies the artifact bytes against the manifest's SHA-256 hash,
    /// then writes the artifact to the install directory.
    pub async fn install(
        &self,
        manifest: &ExtensionManifest,
        bytes: &[u8],
    ) -> aura_core::Result<InstalledArtifact> {
        self.verify_hash(manifest, bytes)?;

        let kind_dir = match manifest.kind {
            ExtensionKind::Skill => "skills",
            ExtensionKind::Tool => "tools",
        };

        let artifact_dir = self
            .install_root
            .join(kind_dir)
            .join(&manifest.name)
            .join(&manifest.version);

        tokio::fs::create_dir_all(&artifact_dir)
            .await
            .map_err(|e| {
                AuraError::Config(format!(
                    "failed to create install directory {}: {e}",
                    artifact_dir.display()
                ))
            })?;

        let artifact_path = artifact_dir.join(&manifest.name);
        tokio::fs::write(&artifact_path, bytes).await.map_err(|e| {
            AuraError::Config(format!(
                "failed to write artifact to {}: {e}",
                artifact_path.display()
            ))
        })?;

        Ok(InstalledArtifact {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            path: artifact_path,
            artifact_hash: manifest.artifact_hash.clone(),
        })
    }

    /// Verifies that the SHA-256 hash of `bytes` matches the manifest's `artifact_hash`.
    pub fn verify_hash(&self, manifest: &ExtensionManifest, bytes: &[u8]) -> aura_core::Result<()> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let computed = hex::encode(hasher.finalize());

        if computed != manifest.artifact_hash {
            return Err(AuraError::Security(format!(
                "hash mismatch for {}: expected {}, got {computed}",
                manifest.name, manifest.artifact_hash
            )));
        }

        Ok(())
    }
}

/// Placeholder for signature verification (Phase 1: not yet implemented).
pub struct SignatureVerifier;

impl SignatureVerifier {
    pub fn new() -> Self {
        Self
    }

    /// Stub: signature verification is not yet implemented.
    pub fn verify(&self, _manifest: &ExtensionManifest, _bytes: &[u8]) -> aura_core::Result<()> {
        // Signature verification will be added in a future phase.
        Ok(())
    }
}

impl Default for SignatureVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    fn test_manifest(hash: &str) -> ExtensionManifest {
        ExtensionManifest {
            name: "test-tool".to_string(),
            version: "1.0.0".to_string(),
            artifact_hash: hash.to_string(),
            signature: None,
            source_url: "https://example.com/test-tool.wasm".to_string(),
            kind: ExtensionKind::Tool,
            trust_level: TrustLevel::Installed,
        }
    }

    #[test]
    fn test_verify_hash_success() {
        let data = b"hello world";
        let hash = sha256_hex(data);
        let installer = RegistryInstaller::new(PathBuf::from("/tmp"));
        let manifest = test_manifest(&hash);
        assert!(installer.verify_hash(&manifest, data).is_ok());
    }

    #[test]
    fn test_verify_hash_mismatch() {
        let data = b"hello world";
        let installer = RegistryInstaller::new(PathBuf::from("/tmp"));
        let manifest =
            test_manifest("0000000000000000000000000000000000000000000000000000000000000000");
        let result = installer.verify_hash(&manifest, data);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_install_success() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("registry_install_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        let data = b"wasm binary content";
        let hash = sha256_hex(data);
        let installer = RegistryInstaller::new(dir.clone());
        let manifest = test_manifest(&hash);

        let artifact = installer.install(&manifest, data).await.unwrap();
        assert_eq!(artifact.name, "test-tool");
        assert_eq!(artifact.version, "1.0.0");
        assert!(artifact.path.exists());

        let written = tokio::fs::read(&artifact.path).await.unwrap();
        assert_eq!(written, data);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_install_hash_mismatch() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("registry_install_fail_test");
        let installer = RegistryInstaller::new(dir);
        let manifest = test_manifest("badhash");
        let result = installer.install(&manifest, b"data").await;
        assert!(result.is_err());
    }
}
