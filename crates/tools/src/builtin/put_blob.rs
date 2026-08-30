//! `PutBlob` — stage a local file in the blob store and return its capability
//! reference without attaching it to a user-facing message.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use baybo_model::{ChannelType, TrustLevel};
use baybo_store::BlobStore;
use serde::Deserialize;
use serde_json::{Value, json};

use super::blob_upload::{
    BLOB_MIME_PARAM_DESC, BLOB_SIZE_CLAUSE, BLOB_TOOL_TIMEOUT, LocalBlobFile, MAX_LOCAL_BLOB_BYTES,
    MAX_LOCAL_BLOB_MIB, path_progress_label, path_read_access, resolve_mime_type,
};
use crate::{
    ResourceAccess, Tool, ToolCapability, ToolContext, ToolError, ToolManifest, ToolOutput,
};

const TOOL_NAME: &str = "PutBlob";
const MAX_BYTES: u64 = MAX_LOCAL_BLOB_BYTES;
const DESCRIPTION_TEMPLATE: &str = r#"Store a local file in BlobStore and return its capability id for another tool or response protocol to reference. This does NOT send or attach the file to the user; use AttachFile when the file itself should appear in the final reply. {{size_clause}}

The returned `blob_id` is a bearer read capability; expose it only through the protocol that requested it."#;

static DESCRIPTION: LazyLock<String> =
    LazyLock::new(|| DESCRIPTION_TEMPLATE.replace("{{size_clause}}", BLOB_SIZE_CLAUSE.as_str()));

struct PutBlobTool {
    blob_store: Arc<dyn BlobStore>,
}

impl PutBlobTool {
    fn new(blob_store: Arc<dyn BlobStore>) -> Self {
        Self { blob_store }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    path: PathBuf,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    max_bytes: Option<u64>,
}

#[async_trait]
impl Tool for PutBlobTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path. Sensitive paths (SSH keys, .env, …) are blocked."
                },
                "mime_type": {
                    "type": "string",
                    "description": BLOB_MIME_PARAM_DESC
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_BYTES,
                    "description": format!("Smaller upload cap for the calling protocol; `0` uses the default {MAX_LOCAL_BLOB_MIB} MiB cap.")
                }
            },
            "required": ["path"]
        })
    }

    fn max_timeout(&self) -> Duration {
        BLOB_TOOL_TIMEOUT
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        path_progress_label(params)
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        path_read_access(params)
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let params: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let max_bytes = requested_limit(params.max_bytes)?;
        let source = LocalBlobFile::inspect(&params.path, TOOL_NAME, "store", max_bytes).await?;
        let mime_type = resolve_mime_type(params.mime_type, &params.path)?;
        let blob = source
            .put(self.blob_store.as_ref(), &mime_type, max_bytes)
            .await?;

        Ok(ToolOutput::Json(json!({
            "blob_id": blob.blob_id,
            "mime_type": mime_type,
            "size_bytes": source.size(),
        })))
    }
}

fn requested_limit(requested: Option<u64>) -> crate::Result<u64> {
    match requested {
        Some(0) => Ok(MAX_BYTES),
        Some(value) if value > MAX_BYTES => Err(ToolError::InvalidParams(format!(
            "max_bytes {value} exceeds the {MAX_BYTES}-byte blob cap"
        ))),
        Some(value) => Ok(value),
        None => Ok(MAX_BYTES),
    }
}

pub(super) fn tool(blob_store: Arc<dyn BlobStore>) -> (Arc<dyn Tool>, ToolManifest) {
    let put_blob = PutBlobTool::new(blob_store);
    let manifest = ToolManifest {
        name: put_blob.name().to_string(),
        description: put_blob.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: put_blob.parameters_schema(),
        capabilities: vec![ToolCapability::ReadFile],
        channels: vec![ChannelType::owner()],
    };
    (Arc::new(put_blob), manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_storage::test_support::MemoryBlobStore;

    #[tokio::test]
    async fn stores_arbitrary_bytes_and_returns_only_a_structured_reference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.data");
        let bytes = b"arbitrary bytes";
        tokio::fs::write(&path, bytes).await.unwrap();
        let store = Arc::new(MemoryBlobStore::new());
        let put_blob = PutBlobTool::new(Arc::clone(&store) as Arc<dyn BlobStore>);

        let output = put_blob
            .execute(
                json!({
                    "path": path,
                    "mime_type": "application/x-example",
                    "max_bytes": bytes.len(),
                }),
                &ToolContext::for_test(),
            )
            .await
            .unwrap();
        let ToolOutput::Json(reference) = output else {
            panic!("expected JSON reference, got {output:?}");
        };
        let blob_id = reference["blob_id"].as_str().expect("blob id");

        assert_eq!(reference["mime_type"], "application/x-example");
        assert_eq!(reference["size_bytes"], bytes.len());
        assert_eq!(store.get(blob_id).await.unwrap(), bytes);
        assert!(!reference.to_string().contains("baybo-html"));
    }

    #[tokio::test]
    async fn infers_mime_type_from_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.html");
        tokio::fs::write(&path, b"<!doctype html>").await.unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let put_blob = PutBlobTool::new(store);

        let output = put_blob
            .execute(json!({ "path": path }), &ToolContext::for_test())
            .await
            .unwrap();
        let ToolOutput::Json(reference) = output else {
            panic!("expected JSON reference");
        };
        assert_eq!(reference["mime_type"], "text/html");
    }

    #[tokio::test]
    async fn enforces_path_safety_and_requested_caps() {
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let put_blob = PutBlobTool::new(store);
        let ctx = ToolContext::for_test();

        let relative = put_blob
            .execute(json!({ "path": "payload.bin" }), &ctx)
            .await
            .unwrap_err();
        assert!(relative.to_string().contains("absolute"));

        let sensitive = put_blob
            .execute(json!({ "path": "/tmp/.env" }), &ctx)
            .await
            .unwrap_err();
        assert!(sensitive.to_string().contains("sensitive"));

        let dir = tempfile::tempdir().unwrap();
        let not_file = put_blob
            .execute(json!({ "path": dir.path() }), &ctx)
            .await
            .unwrap_err();
        assert!(not_file.to_string().contains("not a regular file"));

        let path = dir.path().join("bounded.bin");
        tokio::fs::write(&path, b"12345").await.unwrap();
        let too_large = put_blob
            .execute(json!({ "path": &path, "max_bytes": 4 }), &ctx)
            .await
            .unwrap_err();
        assert!(too_large.to_string().contains("4-byte cap"));

        let default_cap = put_blob
            .execute(json!({ "path": &path, "max_bytes": 0 }), &ctx)
            .await
            .unwrap();
        assert!(matches!(default_cap, ToolOutput::Json(_)));

        for invalid in [MAX_BYTES + 1] {
            let error = put_blob
                .execute(
                    json!({ "path": "/tmp/payload.bin", "max_bytes": invalid }),
                    &ctx,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, ToolError::InvalidParams(_)));
        }
    }

    #[test]
    fn manifest_is_owner_only() {
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let (_, manifest) = tool(store);

        assert_eq!(manifest.channels, vec![ChannelType::owner()]);
        assert!(manifest.allows_channel(&ChannelType::owner()));
        assert!(!manifest.allows_channel(&ChannelType::telegram()));
    }
}
