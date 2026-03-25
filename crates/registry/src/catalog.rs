use serde::{Deserialize, Serialize};

use crate::{ExtensionKind, ExtensionManifest};
use aura_core::TrustLevel;

/// An entry in the extension catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub version: String,
    pub description: String,
    pub kind: ExtensionKind,
    pub artifact_hash: String,
    pub signature: Option<String>,
    pub source_url: String,
    pub trust_level: TrustLevel,
}

impl CatalogEntry {
    /// Convert this catalog entry into an [`ExtensionManifest`].
    pub fn to_manifest(&self) -> ExtensionManifest {
        ExtensionManifest {
            name: self.name.clone(),
            version: self.version.clone(),
            artifact_hash: self.artifact_hash.clone(),
            signature: self.signature.clone(),
            source_url: self.source_url.clone(),
            kind: self.kind.clone(),
            trust_level: self.trust_level.clone(),
        }
    }
}

/// A catalog of available extensions loaded from a JSON index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub entries: Vec<CatalogEntry>,
}

impl Catalog {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Load a catalog from JSON bytes.
    pub fn from_json(data: &[u8]) -> aura_core::Result<Self> {
        serde_json::from_slice(data).map_err(|e| {
            aura_core::AuraError::Serialization(format!("failed to parse catalog: {e}"))
        })
    }

    /// Load a catalog from a file path.
    pub async fn from_file(path: &std::path::Path) -> aura_core::Result<Self> {
        let data = tokio::fs::read(path).await.map_err(|e| {
            aura_core::AuraError::Config(format!(
                "failed to read catalog file {}: {e}",
                path.display()
            ))
        })?;
        Self::from_json(&data)
    }

    /// Search for extensions by name (case-insensitive substring match).
    pub fn search(&self, query: &str) -> Vec<&CatalogEntry> {
        let query_lower = query.to_lowercase();
        self.entries
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&query_lower))
            .collect()
    }

    /// Get the latest version of an extension by name.
    pub fn get_latest(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries
            .iter()
            .filter(|e| e.name == name)
            .max_by(|a, b| a.version.cmp(&b.version))
    }

    /// Get a specific version of an extension.
    pub fn get_version(&self, name: &str, version: &str) -> Option<&CatalogEntry> {
        self.entries
            .iter()
            .find(|e| e.name == name && e.version == version)
    }

    /// Add an entry to the catalog.
    pub fn add(&mut self, entry: CatalogEntry) {
        self.entries.push(entry);
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> &'static [u8] {
        br#"{
            "entries": [
                {
                    "name": "web-search",
                    "version": "1.0.0",
                    "description": "Search the web",
                    "kind": "Tool",
                    "artifact_hash": "abc123",
                    "signature": null,
                    "source_url": "https://example.com/web-search.wasm",
                    "trust_level": "Installed"
                },
                {
                    "name": "web-search",
                    "version": "2.0.0",
                    "description": "Search the web v2",
                    "kind": "Tool",
                    "artifact_hash": "def456",
                    "signature": "sig123",
                    "source_url": "https://example.com/web-search-2.wasm",
                    "trust_level": "Trusted"
                },
                {
                    "name": "code-review",
                    "version": "0.5.0",
                    "description": "Review code changes",
                    "kind": "Skill",
                    "artifact_hash": "ghi789",
                    "signature": null,
                    "source_url": "https://example.com/code-review.wasm",
                    "trust_level": "Installed"
                }
            ]
        }"#
    }

    #[test]
    fn test_from_json() {
        let catalog = Catalog::from_json(sample_json()).unwrap();
        assert_eq!(catalog.entries.len(), 3);
        assert_eq!(catalog.entries[0].name, "web-search");
        assert_eq!(catalog.entries[2].kind, ExtensionKind::Skill);
    }

    #[test]
    fn test_from_json_invalid() {
        let result = Catalog::from_json(b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_search() {
        let catalog = Catalog::from_json(sample_json()).unwrap();

        let results = catalog.search("web");
        assert_eq!(results.len(), 2);

        let results = catalog.search("WEB");
        assert_eq!(results.len(), 2);

        let results = catalog.search("code");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "code-review");

        let results = catalog.search("nonexistent");
        assert!(results.is_empty());
    }

    #[test]
    fn test_get_latest() {
        let catalog = Catalog::from_json(sample_json()).unwrap();

        let latest = catalog.get_latest("web-search");
        assert!(latest.is_some());
        assert_eq!(latest.unwrap().version, "2.0.0");

        assert!(catalog.get_latest("nonexistent").is_none());
    }

    #[test]
    fn test_get_version() {
        let catalog = Catalog::from_json(sample_json()).unwrap();

        let entry = catalog.get_version("web-search", "1.0.0");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().artifact_hash, "abc123");

        assert!(catalog.get_version("web-search", "9.9.9").is_none());
        assert!(catalog.get_version("nonexistent", "1.0.0").is_none());
    }

    #[test]
    fn test_to_manifest() {
        let catalog = Catalog::from_json(sample_json()).unwrap();
        let manifest = catalog.entries[0].to_manifest();
        assert_eq!(manifest.name, "web-search");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.artifact_hash, "abc123");
        assert_eq!(manifest.kind, ExtensionKind::Tool);
    }

    #[test]
    fn test_add_entry() {
        let mut catalog = Catalog::new();
        assert!(catalog.entries.is_empty());

        catalog.add(CatalogEntry {
            name: "test".to_string(),
            version: "1.0.0".to_string(),
            description: "A test extension".to_string(),
            kind: ExtensionKind::Tool,
            artifact_hash: "hash".to_string(),
            signature: None,
            source_url: "https://example.com/test.wasm".to_string(),
            trust_level: TrustLevel::Installed,
        });

        assert_eq!(catalog.entries.len(), 1);
    }

    #[test]
    fn test_default() {
        let catalog = Catalog::default();
        assert!(catalog.entries.is_empty());
    }
}
