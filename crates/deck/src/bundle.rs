//! The on-disk card bundle: `workspace/deck/<uuid>/`.
//!
//! Four plain files, chosen over blob-store versioning so the agent
//! iterates with its ordinary file tools and the operator can read,
//! hand-edit, diff, and git the bundle. Manifest values are install-time
//! defaults only — after install the `deck_cards` row is authoritative
//! for title/size/layout.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use baybo_store::DeckSize;

use crate::error::{DeckError, Result};
use crate::spec::CardSpec;

pub const MANIFEST_FILE: &str = "manifest.json";
pub const OPENAPI_FILE: &str = "openapi.json";
pub const SERVICE_FILE: &str = "service.js";
pub const CARD_FILE: &str = "card.html";

/// Byte cap per agent-written source file (`service.js`, `card.html`).
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
/// Byte cap on `manifest.json`.
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024;
/// Floor on a card's declared emit interval; the emit clamp enforces
/// `max(declared, floor)`.
pub const MIN_EMIT_INTERVAL_FLOOR_SECS: u64 = 10;

/// The service-side SDK version materialized by the current gateway. A
/// card whose recorded stamp differs gets a re-dry-run at boot (the
/// post-upgrade re-admission) instead of a silent behavior change.
pub const SDK_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefreshSpec {
    /// The op the dry-run gate invokes (and the card's own timer
    /// typically re-runs). Must exist in `openapi.json`.
    pub op: String,
    /// Params for the dry-run invocation of `op`. Validated against the
    /// admission contract like any gateway-crossing call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// The card's declared emit floor: the gateway accepts at most one
    /// emit per this many seconds (clamped up to
    /// [`MIN_EMIT_INTERVAL_FLOOR_SECS`]); excess emits coalesce to
    /// latest. An event-driven card declares its floor honestly without
    /// promising a cadence.
    pub min_emit_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeckManifest {
    /// Install-time default; `deck_cards.title` is authoritative after.
    pub title: String,
    /// Install-time default; layout writes own it after.
    #[serde(with = "size_str")]
    pub size: DeckSize,
    pub refresh: RefreshSpec,
    /// Which preamble version installed this card. Stamped by the
    /// installer (agent-provided values are overwritten); informational
    /// plus the boot-time upgrade-detection input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk: Option<u32>,
}

mod size_str {
    use baybo_store::DeckSize;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(size: &DeckSize, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(size.as_str())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DeckSize, D::Error> {
        let raw = String::deserialize(d)?;
        DeckSize::parse(&raw)
            .ok_or_else(|| D::Error::custom(format!("size must be small|wide|large, got `{raw}`")))
    }
}

/// A loaded, statically-validated bundle.
pub struct DeckBundle {
    pub dir: PathBuf,
    pub manifest: DeckManifest,
    pub spec: CardSpec,
    pub card_html: String,
    pub spec_hash: String,
}

impl DeckBundle {
    pub fn service_js_path(&self) -> PathBuf {
        self.dir.join(SERVICE_FILE)
    }

    /// The effective emit floor after clamping to the gateway-wide floor.
    pub fn emit_interval_secs(&self) -> u64 {
        self.manifest
            .refresh
            .min_emit_interval_secs
            .max(MIN_EMIT_INTERVAL_FLOOR_SECS)
    }
}

fn read_capped(dir: &Path, name: &str, cap: usize) -> Result<Vec<u8>> {
    let path = dir.join(name);
    let bytes = std::fs::read(&path)
        .map_err(|e| DeckError::InvalidBundle(format!("cannot read {name}: {e}")))?;
    if bytes.len() > cap {
        return Err(DeckError::InvalidBundle(format!(
            "{name} exceeds {cap} bytes"
        )));
    }
    Ok(bytes)
}

/// Load + statically validate a bundle directory. This is the shared
/// front half of the dry-run gate — everything here is knowable without
/// executing the card.
pub fn load_bundle(dir: &Path) -> Result<DeckBundle> {
    let manifest_bytes = read_capped(dir, MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
    let manifest: DeckManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| DeckError::InvalidBundle(format!("{MANIFEST_FILE}: {e}")))?;
    if manifest.title.trim().is_empty() {
        return Err(DeckError::InvalidBundle("manifest title is empty".into()));
    }
    if manifest.title.chars().count() > 120 {
        return Err(DeckError::InvalidBundle(
            "manifest title exceeds 120 chars".into(),
        ));
    }

    let spec_bytes = read_capped(dir, OPENAPI_FILE, crate::spec::MAX_SPEC_BYTES)?;
    let spec = CardSpec::parse(&spec_bytes)?;
    if !spec.has_op(&manifest.refresh.op) {
        return Err(DeckError::InvalidBundle(format!(
            "refresh op `{}` is not declared in {OPENAPI_FILE}",
            manifest.refresh.op
        )));
    }
    spec.validate_call(
        &manifest.refresh.op,
        manifest.refresh.params.as_ref().unwrap_or(&Value::Null),
    )
    .map_err(|e| DeckError::InvalidBundle(format!("refresh params: {e}")))?;

    let service_js = read_capped(dir, SERVICE_FILE, MAX_SOURCE_BYTES)?;
    let card_html_bytes = read_capped(dir, CARD_FILE, MAX_SOURCE_BYTES)?;
    let card_html = String::from_utf8(card_html_bytes.clone())
        .map_err(|_| DeckError::InvalidBundle(format!("{CARD_FILE} is not UTF-8")))?;

    let mut hasher = Sha256::new();
    for bytes in [&manifest_bytes, &spec_bytes, &service_js, &card_html_bytes] {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    let spec_hash = hex::encode(hasher.finalize());

    Ok(DeckBundle {
        dir: dir.to_path_buf(),
        manifest,
        spec,
        card_html,
        spec_hash,
    })
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    use super::*;
    use std::path::Path;

    /// Write a minimal valid bundle into `dir`. `service_body` is the JS
    /// module source.
    pub fn write_bundle(dir: &Path, title: &str, service_body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::json!({
                "title": title,
                "size": "wide",
                "refresh": {"op": "refresh", "min_emit_interval_secs": 60}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join(OPENAPI_FILE),
            serde_json::json!({
                "openapi": "3.1.0",
                "paths": {"/refresh": {"get": {"x-baybo-retryable": true}}}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.join(SERVICE_FILE), service_body).unwrap();
        std::fs::write(dir.join(CARD_FILE), "<div id=x></div>").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_valid_bundle_and_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        test_fixtures::write_bundle(tmp.path(), "t", "export const ops = {};");
        let b = load_bundle(tmp.path()).unwrap();
        assert_eq!(b.manifest.title, "t");
        assert_eq!(b.manifest.size, DeckSize::Wide);
        assert_eq!(b.emit_interval_secs(), 60);
        assert_eq!(b.spec_hash.len(), 64);

        // Hash moves with content.
        let h1 = b.spec_hash.clone();
        std::fs::write(tmp.path().join(SERVICE_FILE), "export const ops = {a:1};").unwrap();
        let b2 = load_bundle(tmp.path()).unwrap();
        assert_ne!(h1, b2.spec_hash);
    }

    #[test]
    fn rejects_bad_manifests() {
        let tmp = tempfile::tempdir().unwrap();
        test_fixtures::write_bundle(tmp.path(), "t", "export const ops = {};");

        // Refresh op not in the spec.
        std::fs::write(
            tmp.path().join(MANIFEST_FILE),
            serde_json::json!({
                "title": "t", "size": "wide",
                "refresh": {"op": "ghost", "min_emit_interval_secs": 60}
            })
            .to_string(),
        )
        .unwrap();
        assert!(load_bundle(tmp.path()).is_err());

        // Unknown size.
        std::fs::write(
            tmp.path().join(MANIFEST_FILE),
            serde_json::json!({
                "title": "t", "size": "huge",
                "refresh": {"op": "refresh", "min_emit_interval_secs": 60}
            })
            .to_string(),
        )
        .unwrap();
        assert!(load_bundle(tmp.path()).is_err());

        // Missing file.
        std::fs::write(
            tmp.path().join(MANIFEST_FILE),
            serde_json::json!({
                "title": "t", "size": "wide",
                "refresh": {"op": "refresh", "min_emit_interval_secs": 60}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::remove_file(tmp.path().join(CARD_FILE)).unwrap();
        assert!(load_bundle(tmp.path()).is_err());
    }

    #[test]
    fn emit_floor_clamps_up() {
        let tmp = tempfile::tempdir().unwrap();
        test_fixtures::write_bundle(tmp.path(), "t", "export const ops = {};");
        std::fs::write(
            tmp.path().join(MANIFEST_FILE),
            serde_json::json!({
                "title": "t", "size": "small",
                "refresh": {"op": "refresh", "min_emit_interval_secs": 1}
            })
            .to_string(),
        )
        .unwrap();
        let b = load_bundle(tmp.path()).unwrap();
        assert_eq!(b.emit_interval_secs(), MIN_EMIT_INTERVAL_FLOOR_SECS);
    }
}
