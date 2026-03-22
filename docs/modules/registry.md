# registry - Extension Registry and Installation Governance

## 1. Module Overview

The `registry` crate is responsible for discovery, download, verification, installation, and upgrade of Skill and Tool extensions. It does not execute extensions. Its role is to turn an "external artifact" into a "governable artifact."

Core responsibilities:

- Maintain the registry index
- Download and verify extension artifacts
- Record source, version, hash, signature, and trust level
- Install artifacts into a local directory for `tools` and `skills` to consume

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Purpose |
|-----------|------|
| `core` | Error types and foundational metadata |

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `serde` / `serde_json` | Registry index and manifest parsing |
| `reqwest` or equivalent HTTP client | Downloading registries and artifacts |
| `sha2` / signature libraries | Hash and signature verification |

---

## 3. Public Interfaces

```rust
pub struct ExtensionManifest {
    pub name: String,
    pub version: String,
    pub artifact_hash: String,
    pub signature: Option<String>,
    pub source_url: String,
    pub kind: ExtensionKind,
    pub trust_level: TrustLevel,
}

pub enum ExtensionKind {
    Skill,
    Tool,
}

pub struct RegistryInstaller {
    verifier: SignatureVerifier,
    install_root: PathBuf,
}
```

Recommended additional interfaces:

```rust
impl RegistryInstaller {
    pub async fn install(&self, manifest: &ExtensionManifest) -> Result<InstalledArtifact>;
    pub async fn upgrade(&self, name: &str, target_version: &str) -> Result<InstalledArtifact>;
    pub fn verify(&self, manifest: &ExtensionManifest, bytes: &[u8]) -> Result<()>;
}
```

---

## 4. Implementation Details

### 4.1 Governance Entry Point

Installation flow:

```text
catalog index
    │
    ▼
ExtensionManifest
    │
    ▼
download
    │
    ▼
hash / signature verify
    │
    ▼
install_root
    │
    ├── consumed by tools
    └── consumed by skills
```

### 4.2 Source of Trust Levels

`registry` is responsible for mapping external sources into governance fields:

- `source_url`
- `artifact_hash`
- `signature`
- `trust_level`

`skills` and `tools` should consume these fields directly rather than inferring trustworthiness on their own.

### 4.3 Installation Constraints

- Artifacts that fail verification must not be written to disk in an executable state
- The installation directory must be separated from user-authored workspace directories
- Upgrades should preserve metadata for old versions to support Trace replay and troubleshooting

---

## 5. Collaboration with Other Modules

| Module | Collaboration |
|------|---------|
| `skills` | Provides source, version, and trust level for installed skills |
| `tools` | Provides source, version, and artifact hash for installed tools |
| `trace` | Needs registry metadata for provenance during execution |
| `workspace` | Local workspace extensions and registry install directories should be separate |

---

## 6. Implementation Recommendations

- Separate "installable" from "allowed to auto-execute"; successful installation does not imply high privileges
- Registry download failures should not block already installed extensions from being used
- Signature verification can be optional at first, but hash verification should be mandatory
