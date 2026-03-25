use sha2::{Digest, Sha256};

use crate::ExtensionManifest;

/// Verifies extension artifact integrity through hash and optional signature checks.
pub struct ArtifactVerifier {
    /// Whether signature verification is enforced.
    require_signature: bool,
}

impl ArtifactVerifier {
    pub fn new() -> Self {
        Self {
            require_signature: false,
        }
    }

    pub fn with_signature_requirement(require_signature: bool) -> Self {
        Self { require_signature }
    }

    /// Verify the SHA-256 hash of artifact bytes against the manifest.
    pub fn verify_hash(&self, manifest: &ExtensionManifest, bytes: &[u8]) -> aura_core::Result<()> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let computed = hex::encode(hasher.finalize());

        if computed != manifest.artifact_hash {
            return Err(aura_core::AuraError::Security(format!(
                "hash mismatch for '{}': expected {}, computed {computed}",
                manifest.name, manifest.artifact_hash
            )));
        }
        Ok(())
    }

    /// Verify the signature of artifact bytes against the manifest.
    /// Currently a stub that always succeeds unless `require_signature` is set
    /// and no signature is present.
    pub fn verify_signature(
        &self,
        manifest: &ExtensionManifest,
        _bytes: &[u8],
    ) -> aura_core::Result<()> {
        if self.require_signature && manifest.signature.is_none() {
            return Err(aura_core::AuraError::Security(format!(
                "signature required but not present for '{}'",
                manifest.name
            )));
        }
        // Actual cryptographic signature verification to be implemented in a future phase.
        Ok(())
    }

    /// Run all verification checks (hash + optional signature).
    pub fn verify_all(&self, manifest: &ExtensionManifest, bytes: &[u8]) -> aura_core::Result<()> {
        self.verify_hash(manifest, bytes)?;
        self.verify_signature(manifest, bytes)?;
        Ok(())
    }
}

impl Default for ArtifactVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExtensionKind;
    use aura_core::TrustLevel;
    use sha2::{Digest, Sha256};

    fn sha256_hex(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    fn test_manifest(hash: &str, signature: Option<String>) -> ExtensionManifest {
        ExtensionManifest {
            name: "test-tool".to_string(),
            version: "1.0.0".to_string(),
            artifact_hash: hash.to_string(),
            signature,
            source_url: "https://example.com/test-tool.wasm".to_string(),
            kind: ExtensionKind::Tool,
            trust_level: TrustLevel::Installed,
        }
    }

    #[test]
    fn test_verify_hash_success() {
        let data = b"hello world";
        let hash = sha256_hex(data);
        let verifier = ArtifactVerifier::new();
        let manifest = test_manifest(&hash, None);
        assert!(verifier.verify_hash(&manifest, data).is_ok());
    }

    #[test]
    fn test_verify_hash_failure() {
        let data = b"hello world";
        let verifier = ArtifactVerifier::new();
        let manifest = test_manifest("badhash", None);
        let result = verifier.verify_hash(&manifest, data);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_signature_not_required() {
        let verifier = ArtifactVerifier::new();
        let manifest = test_manifest("hash", None);
        assert!(verifier.verify_signature(&manifest, b"data").is_ok());
    }

    #[test]
    fn test_verify_signature_required_and_present() {
        let verifier = ArtifactVerifier::with_signature_requirement(true);
        let manifest = test_manifest("hash", Some("sig".to_string()));
        assert!(verifier.verify_signature(&manifest, b"data").is_ok());
    }

    #[test]
    fn test_verify_signature_required_but_missing() {
        let verifier = ArtifactVerifier::with_signature_requirement(true);
        let manifest = test_manifest("hash", None);
        let result = verifier.verify_signature(&manifest, b"data");
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_all_success() {
        let data = b"test content";
        let hash = sha256_hex(data);
        let verifier = ArtifactVerifier::new();
        let manifest = test_manifest(&hash, None);
        assert!(verifier.verify_all(&manifest, data).is_ok());
    }

    #[test]
    fn test_verify_all_hash_fails() {
        let verifier = ArtifactVerifier::new();
        let manifest = test_manifest("badhash", None);
        assert!(verifier.verify_all(&manifest, b"data").is_err());
    }

    #[test]
    fn test_verify_all_signature_fails() {
        let data = b"test content";
        let hash = sha256_hex(data);
        let verifier = ArtifactVerifier::with_signature_requirement(true);
        let manifest = test_manifest(&hash, None);
        assert!(verifier.verify_all(&manifest, data).is_err());
    }
}
