//! `SendFile` — stream a local file to the user as a channel attachment.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use aura_model::{ContentBlock, TrustLevel};
use aura_store::BlobStore;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::io::ReaderStream;

use super::paths::require_absolute;
use crate::{
    ResourceAccess, Tool, ToolCapability, ToolContext, ToolError, ToolManifest, ToolOutput,
};

const MAX_BYTES: u64 = 100 * 1024 * 1024;
const MAX_MIB: u64 = MAX_BYTES / 1024 / 1024;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Send a local file to the user as a channel attachment (Telegram \
         document/photo/video, WeChat file, …). The file is streamed from \
         disk; supports any MIME up to {MAX_MIB} MiB. Sensitive paths (SSH \
         keys, .env, /etc/shadow, …) are blocked. After calling, write \
         only a brief textual confirmation — do NOT paste the file's \
         contents into your reply. Use this instead of pasting binary or \
         large text into messages.\n\n\
         PATHS: `path` MUST be an absolute filesystem path. Relative paths \
         are rejected."
    )
});

pub struct SendFileTool {
    blob_store: Arc<dyn BlobStore>,
}

impl SendFileTool {
    pub fn new(blob_store: Arc<dyn BlobStore>) -> Self {
        Self { blob_store }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    path: PathBuf,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    mime: Option<String>,
}

#[async_trait]
impl Tool for SendFileTool {
    fn name(&self) -> &str {
        "SendFile"
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
                    "description": "Absolute path to the file to send."
                },
                "filename": {
                    "type": "string",
                    "description": "Optional override for the filename shown to the recipient. Defaults to the path's basename."
                },
                "mime": {
                    "type": "string",
                    "description": "Optional MIME type override. Defaults to an extension-based guess; falls back to application/octet-stream."
                }
            },
            "required": ["path"]
        })
    }

    fn max_timeout(&self) -> Duration {
        // Streams up to 100 MiB into BlobStore. On a slow disk the
        // copy alone can take tens of seconds, so the trait-default
        // 30 s is tight; 60 s covers the worst case while still
        // bounding a stuck blob backend.
        Duration::from_secs(60)
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("path")
            .and_then(Value::as_str)
            .map(|s| crate::progress::preview_path(Path::new(s)))
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        params
            .get("path")
            .and_then(Value::as_str)
            .map(|path| {
                vec![ResourceAccess::ReadFile {
                    path: PathBuf::from(path),
                }]
            })
            .unwrap_or_default()
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.path, "SendFile", "path")?;

        if aura_security::is_sensitive_path(&p.path) {
            tracing::warn!(path = %p.path.display(), "SendFile refused sensitive path");
            return Err(ToolError::Execution(format!(
                "refused to send sensitive path {} — credential-bearing files are blocked by security policy",
                p.path.display()
            )));
        }

        let meta = tokio::fs::metadata(&p.path)
            .await
            .map_err(|e| ToolError::Execution(format!("stat {}: {e}", p.path.display())))?;
        if !meta.is_file() {
            return Err(ToolError::Execution(format!(
                "{} is not a regular file",
                p.path.display()
            )));
        }
        let size = meta.len();
        if size > MAX_BYTES {
            return Err(ToolError::Execution(format!(
                "{} is {size} bytes, exceeds the {MAX_BYTES}-byte cap",
                p.path.display()
            )));
        }

        let filename = p.filename.unwrap_or_else(|| {
            p.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string())
        });
        let mime = p.mime.unwrap_or_else(|| guess_mime(&p.path).to_string());

        let file = tokio::fs::File::open(&p.path)
            .await
            .map_err(|e| ToolError::Execution(format!("open {}: {e}", p.path.display())))?;
        let stream = ReaderStream::new(file).boxed();

        let blob_ref = self
            .blob_store
            .put_stream(stream, &mime, None, MAX_BYTES)
            .await
            .map_err(|e| ToolError::Execution(format!("blob upload: {e}")))?;

        let attachment = ContentBlock::File {
            blob: blob_ref,
            filename: filename.clone(),
            mime_type: mime.clone(),
        };

        Ok(ToolOutput::WithAttachments {
            text: format!("Sent {filename} ({size} bytes, {mime}) to user."),
            attachments: vec![attachment],
        })
    }
}

/// Build the `(Arc<dyn Tool>, ToolManifest)` pair for registration.
pub fn tool(blob_store: Arc<dyn BlobStore>) -> (Arc<dyn Tool>, ToolManifest) {
    let send = SendFileTool::new(blob_store);
    let manifest = ToolManifest {
        name: send.name().to_string(),
        description: send.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: send.parameters_schema(),
        capabilities: vec![ToolCapability::ReadFile],
    };
    (Arc::new(send), manifest)
}

fn guess_mime(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("gz" | "tgz") => "application/gzip",
        Some("json") => "application/json",
        Some("yaml" | "yml") => "application/yaml",
        Some("xml") => "application/xml",
        Some("txt" | "log" | "md") => "text/plain",
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("csv") => "text/csv",
        Some("js" | "mjs") => "application/javascript",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("mp3") => "audio/mpeg",
        Some("ogg" | "opus") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("m4a") => "audio/mp4",
        Some("flac") => "audio/flac",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, User};
    use aura_storage::test_support::MemoryBlobStore;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "test".into(),
            job_id: aura_model::JobId::default(),
            span_id: aura_model::SpanId::default(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            cancellation_token: CancellationToken::new(),
            workspace_root: PathBuf::from("/tmp"),
            workspace_paths: aura_workspace::WorkspacePaths::new("/tmp"),
            sandbox: None,
            approval: None,
            notifier: None,
            events: crate::noop_event_sink(),
            llm: None,
            secrets: None,
            virtual_reads: None,
        }
    }

    #[tokio::test]
    async fn sends_pdf_with_inferred_mime() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("report.pdf");
        tokio::fs::write(&p, b"%PDF-1.4 fake").await.unwrap();
        let mem = Arc::new(MemoryBlobStore::new());
        let tool = SendFileTool::new(Arc::clone(&mem) as Arc<dyn BlobStore>);

        let out = tool.execute(json!({ "path": p }), &ctx()).await.unwrap();
        let ToolOutput::WithAttachments { text, attachments } = out else {
            panic!("expected WithAttachments");
        };
        assert_eq!(attachments.len(), 1);
        match &attachments[0] {
            ContentBlock::File {
                filename,
                mime_type,
                ..
            } => {
                assert_eq!(filename, "report.pdf");
                assert_eq!(mime_type, "application/pdf");
            }
            other => panic!("expected File block, got {other:?}"),
        }
        assert!(text.contains("report.pdf"));
        assert_eq!(mem.len(), 1);
    }

    #[tokio::test]
    async fn refuses_sensitive_path() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        tokio::fs::create_dir(&ssh).await.unwrap();
        let key = ssh.join("id_rsa");
        tokio::fs::write(&key, "FAKE").await.unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = SendFileTool::new(store);

        let err = tool
            .execute(json!({ "path": key }), &ctx())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("sensitive path") || msg.contains("credential-bearing"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_relative_path() {
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = SendFileTool::new(store);
        let err = tool
            .execute(json!({ "path": "rel.txt" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn refuses_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = SendFileTool::new(store);

        let err = tool
            .execute(json!({ "path": dir.path() }), &ctx())
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("not a regular file"), "got: {msg}");
    }

    #[tokio::test]
    async fn explicit_mime_and_filename_override() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("blob.bin");
        tokio::fs::write(&p, b"raw bytes").await.unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = SendFileTool::new(store);

        let out = tool
            .execute(
                json!({
                    "path": p,
                    "filename": "report.bin",
                    "mime": "application/x-custom"
                }),
                &ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::WithAttachments { attachments, .. } = out else {
            panic!("expected WithAttachments");
        };
        match &attachments[0] {
            ContentBlock::File {
                filename,
                mime_type,
                ..
            } => {
                assert_eq!(filename, "report.bin");
                assert_eq!(mime_type, "application/x-custom");
            }
            other => panic!("expected File block, got {other:?}"),
        }
    }
}
