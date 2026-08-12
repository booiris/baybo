//! `GetBlob` — resolve a `blob_id` to the payload's path on this host.
//!
//! Deliberately one job. It answers where the bytes are and nothing else:
//! no copy, no delivery, no putting the picture in front of the model.
//!
//! Without it `BlobStore` is a write-only port at the tool layer: `AttachFile` and
//! `PutBlob` mint capability ids, and every *other* consumer in the system
//! (the gateway's download route, the LLM request builder, channel sidecars,
//! deck cards, iOS) can spend one — but the agent that mints them could not.
//! An agent holding a user's photo saw the pixels and still had no way to
//! name the file for a subprocess.
//!
//! **This tool writes nothing.** It hands back the store's own path, which
//! the Bash sandbox re-exposes read-only (`sandbox_readable_paths` binds
//! `WorkspacePaths::blobs_dir` back over the `$BAYBO_HOME` mask). Two
//! consequences worth keeping:
//!
//! * **Read-only is not a limitation to relax.** The payload is
//!   content-addressed and shared by every row with the same digest, so a
//!   process editing it in place would rewrite every blob referencing that
//!   content. A caller that needs to modify the bytes copies them somewhere
//!   of its own first.
//! * **The path comes from the store, never from the digest.** The layout
//!   (`<root>/<hex[..2]>/<hex><ext>`) is a frozen persistence format keyed on
//!   the hex alone, so re-deriving it here would both duplicate that format
//!   and drop the `read_token` gate — see the security note on
//!   [`baybo_store::BlobStore::local_path`].

use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{TrustLevel, blob_content_digest};
use baybo_store::{BlobStore, StorageError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    ResourceAccess, Tool, ToolCapability, ToolConcurrency, ToolContext, ToolError, ToolManifest,
    ToolOutput,
};

const TOOL_NAME: &str = "GetBlob";

/// Kept to three claims, because this ships in every session's tool list:
/// what it returns, when to reach for it, and why the path must not be
/// written to. The last one states the hazard — one shared copy — and stops
/// there. A recipe for the situation ("copy it into the work directory
/// first") would be teaching a recovery the model has not needed yet, for a
/// use this tool is not for; the shell's own `Read-only file system` is a
/// better time to learn it. The id's format and the copy-it-whole rule live on the `blob_id`
/// field instead — a parameter's own description is where a model looks
/// when it is filling that parameter in, and saying it twice costs every
/// turn. `PutBlob` goes unmentioned for a subtler reason: it is
/// `channels: [owner]` while this tool is unrestricted, so on a Telegram
/// session "the inverse of PutBlob" would point at a tool the model cannot
/// see.
const DESCRIPTION: &str = r#"Resolve a blob to its file on disk and return the absolute path.

Reach for it whenever something needs a FILE — an external CLI, a skill's script, a Bash pipeline. The path cannot be derived from a blob_id by hand; this is the only way to get one. Ids ride in the `blob` field of an Image / Audio / File block, or in a tool result.

The path is READ-ONLY: it is the single stored copy, shared by every blob with the same content."#;

pub(super) struct GetBlobTool {
    blob_store: Arc<dyn BlobStore>,
}

impl GetBlobTool {
    pub(super) fn new(blob_store: Arc<dyn BlobStore>) -> Self {
        Self { blob_store }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    blob_id: String,
}

#[async_trait]
impl Tool for GetBlobTool {
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
                "blob_id": {
                    "type": "string",
                    "description": "Full capability id, `sha256:<64 hex>.<token>`. Copy it verbatim — a truncated id, or one missing the trailing token, will not resolve."
                }
            },
            "required": ["blob_id"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        let blob_id = params.get("blob_id").and_then(Value::as_str)?;
        let digest = blob_content_digest(blob_id)?;
        Some(digest.chars().take(12).collect())
    }

    /// Empty: this tool creates nothing and touches no path the caller
    /// named. What it returns is a read-only view of a file the store
    /// already owns, gated by the `blob_id` the caller had to hold.
    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        Vec::new()
    }

    /// A `stat` and a path — no writes anywhere, unlike its `AttachFile` /
    /// `PutBlob` siblings, which stage bytes and stay exclusive. Resolving
    /// several attachments at once is the normal case for a turn that is
    /// about to hand them all to one command.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let blob_id = p.blob_id.trim();
        if blob_content_digest(blob_id).is_none() {
            return Err(ToolError::InvalidParams(format!(
                "{blob_id:?} is not a blob id — expected sha256:<64 hex>.<token>"
            )));
        }

        let meta = self.blob_store.stat(blob_id).await.map_err(not_found)?;
        let path = self
            .blob_store
            .local_path(blob_id)
            .await
            .map_err(not_found)?
            .ok_or_else(|| {
                ToolError::Execution(
                    "this blob backend keeps no on-disk payload, so there is no path to hand out"
                        .to_string(),
                )
            })?;

        Ok(ToolOutput::Json(json!({
            "path": path,
            "mime_type": meta.mime_type,
            "size_bytes": meta.size,
        })))
    }
}

/// `NotFound` arrives from three different situations and the store
/// deliberately will not say which: no such row, wrong token, or — from
/// `local_payload`, after the row and token have already checked out — a
/// payload no longer on disk.
///
/// So the message names all three rather than prescribing the fix for one.
/// Telling a model to re-copy its id is a guaranteed wasted turn when the id
/// was exact and the bytes are simply gone, which is the case this tool's
/// own `a_row_without_its_payload_is_not_found` pins.
fn not_found(e: StorageError) -> ToolError {
    match e {
        StorageError::NotFound(_) => ToolError::Execution(
            "no readable blob for that id — it is wrong, or it lost the `.<token>` suffix \
             that carries the read capability, or its bytes are no longer on this host. \
             Re-copy the id from the block it came from; if it was already exact, the \
             payload is gone and retrying will not bring it back."
                .to_string(),
        ),
        other => ToolError::Execution(format!("blob lookup: {other}")),
    }
}

pub(super) fn tool(blob_store: Arc<dyn BlobStore>) -> (Arc<dyn Tool>, ToolManifest) {
    let get_blob = GetBlobTool::new(blob_store);
    let manifest = ToolManifest {
        name: get_blob.name().to_string(),
        description: get_blob.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: get_blob.parameters_schema(),
        capabilities: vec![ToolCapability::ReadFile],
        // Unrestricted, unlike `PutBlob`. That one is owner-gated because
        // `channels: [owner]` is the only enforced gate on MINTING a bearer
        // id; this one spends a capability the caller already holds, and a
        // photo sent from Telegram hits the identical problem.
        channels: Vec::new(),
    };
    (Arc::new(get_blob), manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::ChannelType;
    use baybo_storage::sqlite::{SqliteBlobStore, SqlitePool};
    use baybo_storage::test_support::MemoryBlobStore;
    use baybo_store::BlobStore;

    async fn disk_store(dir: &std::path::Path) -> Arc<SqliteBlobStore> {
        let pool = SqlitePool::open(dir.join("storage.db")).await.unwrap();
        Arc::new(
            SqliteBlobStore::open(pool, dir.join("blobs"))
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn returns_the_stores_own_path_for_the_payload() {
        let dir = tempfile::tempdir().unwrap();
        let store = disk_store(dir.path()).await;
        let bytes = b"\xff\xd8\xff\xe0 not really a jpeg";
        let blob_id = store.put(bytes, "image/jpeg", None).await.unwrap().blob_id;
        let get = GetBlobTool::new(store as Arc<dyn BlobStore>);

        let ToolOutput::Json(payload) = get
            .execute(json!({ "blob_id": blob_id }), &ToolContext::for_test())
            .await
            .unwrap()
        else {
            panic!("expected JSON reference");
        };

        let path = std::path::PathBuf::from(payload["path"].as_str().unwrap());
        assert_eq!(payload["mime_type"], "image/jpeg");
        assert_eq!(payload["size_bytes"], bytes.len());
        assert_eq!(tokio::fs::read(&path).await.unwrap(), bytes);
        // The one stored copy, not a second one made for this call.
        assert!(path.starts_with(dir.path().join("blobs")), "{path:?}");
    }

    /// The digest alone is everything a leaked transcript excerpt exposes,
    /// and it is also the entire on-disk filename — so a path answer is the
    /// one place where skipping the token check would hand out the file.
    #[tokio::test]
    async fn the_read_token_is_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let store = disk_store(dir.path()).await;
        let blob_id = store
            .put(b"secret", "text/plain", None)
            .await
            .unwrap()
            .blob_id;
        let digest = blob_content_digest(&blob_id).unwrap().to_string();
        let get = GetBlobTool::new(store as Arc<dyn BlobStore>);
        let ctx = ToolContext::for_test();

        for forged in [
            format!("sha256:{digest}"),
            format!("sha256:{digest}.wrong-token"),
        ] {
            let error = get
                .execute(json!({ "blob_id": forged }), &ctx)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("no readable blob"),
                "unexpected error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn a_malformed_id_is_rejected_before_any_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let get = GetBlobTool::new(disk_store(dir.path()).await as Arc<dyn BlobStore>);
        let ctx = ToolContext::for_test();

        for malformed in ["", "not-an-id", "sha256:", "sha256:.tok"] {
            let error = get
                .execute(json!({ "blob_id": malformed }), &ctx)
                .await
                .unwrap_err();
            assert!(
                matches!(error, ToolError::InvalidParams(_)),
                "{malformed:?} produced {error}"
            );
        }
    }

    /// A row whose payload was unlinked must not answer with a path to a
    /// file that is not there — the caller hands it straight to a subprocess.
    #[tokio::test]
    async fn a_row_without_its_payload_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = disk_store(dir.path()).await;
        let blob_id = store
            .put(b"gone", "text/plain", None)
            .await
            .unwrap()
            .blob_id;
        let digest = blob_content_digest(&blob_id).unwrap();
        tokio::fs::remove_file(
            dir.path()
                .join("blobs")
                .join(&digest[..2])
                .join(format!("{digest}.txt")),
        )
        .await
        .unwrap();

        let get = GetBlobTool::new(store as Arc<dyn BlobStore>);
        let error = get
            .execute(json!({ "blob_id": blob_id }), &ToolContext::for_test())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no readable blob"), "{error}");
    }

    /// The in-memory fake has no payload on disk. It must say so rather
    /// than inventing a path, which is what a naive digest-join would do.
    #[tokio::test]
    async fn a_backend_without_on_disk_payloads_says_so() {
        let store = Arc::new(MemoryBlobStore::new());
        let blob_id = store
            .put(b"in memory", "text/plain", None)
            .await
            .unwrap()
            .blob_id;
        let get = GetBlobTool::new(store as Arc<dyn BlobStore>);

        let error = get
            .execute(json!({ "blob_id": blob_id }), &ToolContext::for_test())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no on-disk payload"), "{error}");
    }

    #[tokio::test]
    async fn manifest_is_open_to_every_channel_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (_, manifest) = tool(disk_store(dir.path()).await as Arc<dyn BlobStore>);

        assert!(manifest.allows_channel(&ChannelType::owner()));
        assert!(manifest.allows_channel(&ChannelType::telegram()));
        assert_eq!(manifest.capabilities, vec![ToolCapability::ReadFile]);
    }
}
