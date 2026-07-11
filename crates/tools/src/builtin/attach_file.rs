//! `AttachFile` — stream a local file into the blob store and attach it to
//! the turn's final assistant reply.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use baybo_model::{BlobRef, ContentBlock, TrustLevel};
use baybo_store::BlobStore;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::io::ReaderStream;

use super::paths::require_absolute;
use crate::{
    ResourceAccess, Tool, ToolCapability, ToolContext, ToolError, ToolManifest, ToolOutput,
};

const TOOL_NAME: &str = "AttachFile";
const MAX_BYTES: u64 = 100 * 1024 * 1024;
const MAX_MIB: u64 = MAX_BYTES / 1024 / 1024;

const DESCRIPTION_TEMPLATE: &str = r#"Give the user a local file — it arrives as an attachment in the chat. Any MIME type, up to {{max_mib}} MiB. Use it instead of pasting binary or large text into a message, and don't paste the contents after attaching.

DELIVERY: the file attaches to your FINAL reply, not to this call; several calls in one turn share that reply.

PATHS: `path` MUST be absolute. Sensitive paths (SSH keys, .env, /etc/shadow, …) are blocked."#;

static DESCRIPTION: LazyLock<String> =
    LazyLock::new(|| DESCRIPTION_TEMPLATE.replace("{{max_mib}}", &MAX_MIB.to_string()));

pub struct AttachFileTool {
    blob_store: Arc<dyn BlobStore>,
}

impl AttachFileTool {
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
impl Tool for AttachFileTool {
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
                    "description": "Absolute path to the file to attach."
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

        require_absolute(&p.path, TOOL_NAME, "path")?;

        if baybo_security::is_sensitive_path(&p.path) {
            tracing::warn!(path = %p.path.display(), "{TOOL_NAME} refused sensitive path");
            return Err(ToolError::Execution(format!(
                "refused to attach sensitive path {} — credential-bearing files are blocked by security policy",
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

        let duration_ms = probe_media_duration_ms(&p.path, &mime).await;

        let file = tokio::fs::File::open(&p.path)
            .await
            .map_err(|e| ToolError::Execution(format!("open {}: {e}", p.path.display())))?;
        let stream = ReaderStream::new(file).boxed();

        let blob_ref = self
            .blob_store
            .put_stream(stream, &mime, None, MAX_BYTES)
            .await
            .map_err(|e| ToolError::Execution(format!("blob upload: {e}")))?;

        // The agent loop hoists these attachments onto the turn's final
        // assistant message, so the file persists with the transcript and
        // survives a reload. Nothing has reached the user yet — say so, or
        // the model reports a delivery that a cancelled turn never makes.
        Ok(ToolOutput::WithAttachments {
            text: format!("Attached {filename} ({size} bytes, {mime}) to your final reply."),
            attachments: vec![media_block(blob_ref, filename, mime, duration_ms)],
        })
    }
}

/// A track's playback length, read from the container headers — attach time is
/// the only moment the file is in hand server-side, and clients want to show a
/// length before downloading a byte. Audio goes through lofty; video through
/// the container-specific parsers below (lofty reads no video container).
/// Best-effort: an unparseable file just ships without a duration; a zero
/// duration (a container that declares none) counts as unknown.
/// `spawn_blocking` because all three parsers are sync IO.
async fn probe_media_duration_ms(path: &Path, mime: &str) -> Option<u32> {
    let path = path.to_path_buf();
    let mime = mime.to_string();
    tokio::task::spawn_blocking(move || {
        let ms = if mime.starts_with("audio/") {
            audio_duration_ms(&path)
        } else if mime == "video/mp4" || mime == "video/quicktime" {
            mp4_duration_ms(&path)
        } else if mime == "video/webm" || mime == "video/x-matroska" {
            webm_duration_ms(&path)
        } else {
            None
        };
        ms.filter(|&v| v > 0)
    })
    .await
    .ok()
    .flatten()
}

fn audio_duration_ms(path: &Path) -> Option<u32> {
    use lofty::file::AudioFile;
    let file = lofty::read_from_path(path).ok()?;
    u32::try_from(file.properties().duration().as_millis()).ok()
}

/// ISO BMFF (`.mp4` / `.mov` — QuickTime shares the box structure): the
/// `mvhd` movie duration.
fn mp4_duration_ms(path: &Path) -> Option<u32> {
    let file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let reader = std::io::BufReader::new(file);
    let mp4 = mp4::Mp4Reader::read_header(reader, size).ok()?;
    u32::try_from(mp4.duration().as_millis()).ok()
}

fn webm_duration_ms(path: &Path) -> Option<u32> {
    let mkv = matroska::open(path).ok()?;
    let duration = mkv.info.duration?;
    u32::try_from(duration.as_millis()).ok()
}

/// Pick the block variant by MIME, because the variant *is* the wire's
/// `AttachmentKind` (`split_content` maps one to the other) and `kind` is what
/// makes a surface render a thumbnail instead of a paperclip chip. This mirrors
/// `attachmentKind` in `app/ios/web/src/types.ts` and the web chat's own
/// bucketing, which that comment already names the gateway as sharing.
///
/// No surface shows a name BESIDE a rendered image, but the name still rides
/// along: a client sharing or saving the picture needs the real one (else it
/// lands in Photos/Files as `attachment.png`).
fn media_block(
    blob: BlobRef,
    filename: String,
    mime_type: String,
    duration_ms: Option<u32>,
) -> ContentBlock {
    if mime_type.starts_with("image/") {
        ContentBlock::Image {
            blob,
            mime_type,
            filename: Some(filename),
        }
    } else if mime_type.starts_with("audio/") {
        ContentBlock::Audio {
            blob,
            mime_type,
            filename: Some(filename),
            duration_ms,
        }
    } else {
        ContentBlock::File {
            blob,
            filename,
            mime_type,
            duration_ms,
        }
    }
}

/// Build the `(Arc<dyn Tool>, ToolManifest)` pair for registration.
pub fn tool(blob_store: Arc<dyn BlobStore>) -> (Arc<dyn Tool>, ToolManifest) {
    let attach = AttachFileTool::new(blob_store);
    let manifest = ToolManifest {
        name: attach.name().to_string(),
        description: attach.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: attach.parameters_schema(),
        capabilities: vec![ToolCapability::ReadFile],
    };
    (Arc::new(attach), manifest)
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
    use baybo_model::{ChannelType, User};
    use baybo_storage::test_support::MemoryBlobStore;
    use std::time::Duration;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "test".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            workspace_root: PathBuf::from("/tmp"),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            ..ToolContext::for_test()
        }
    }

    #[tokio::test]
    async fn attaches_pdf_with_inferred_mime() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("report.pdf");
        tokio::fs::write(&p, b"%PDF-1.4 fake").await.unwrap();
        let mem = Arc::new(MemoryBlobStore::new());
        let tool = AttachFileTool::new(Arc::clone(&mem) as Arc<dyn BlobStore>);

        let out = tool.execute(json!({ "path": p }), &ctx()).await.unwrap();
        let ToolOutput::WithAttachments { text, attachments } = out else {
            panic!("expected WithAttachments, got {out:?}");
        };
        assert!(text.contains("report.pdf"));

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
        assert_eq!(mem.len(), 1);
    }

    /// The block variant IS the wire's `AttachmentKind`, and `kind` is what
    /// decides thumbnail vs paperclip chip. A `File` block for a PNG showed the
    /// agent's screenshot as `📎 chart.png`.
    #[tokio::test]
    async fn media_is_bucketed_by_mime_not_hardcoded_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = AttachFileTool::new(store);

        let cases: [(&str, &str, &str); 4] = [
            ("chart.png", "image", "image/png"),
            ("diagram.svg", "image", "image/svg+xml"),
            ("note.mp3", "audio", "audio/mpeg"),
            ("report.pdf", "file", "application/pdf"),
        ];
        for (name, want_kind, want_mime) in cases {
            let p = dir.path().join(name);
            tokio::fs::write(&p, b"bytes").await.unwrap();
            let out = tool.execute(json!({ "path": p }), &ctx()).await.unwrap();
            let ToolOutput::WithAttachments { attachments, .. } = out else {
                panic!("expected WithAttachments for {name}");
            };
            let (kind, mime, got_name) = match &attachments[0] {
                ContentBlock::Image {
                    mime_type,
                    filename,
                    ..
                } => ("image", mime_type, filename.clone()),
                ContentBlock::Audio {
                    mime_type,
                    filename,
                    ..
                } => ("audio", mime_type, filename.clone()),
                ContentBlock::File {
                    mime_type,
                    filename,
                    ..
                } => ("file", mime_type, Some(filename.clone())),
                other => panic!("expected media block for {name}, got {other:?}"),
            };
            assert_eq!(kind, want_kind, "{name}");
            assert_eq!(mime, want_mime, "{name}");
            // EVERY kind keeps the real name — an image that loses it reaches the
            // client nameless and gets shared/saved as `attachment.png`.
            assert_eq!(got_name.as_deref(), Some(name), "{name} lost its filename");
        }
    }

    #[tokio::test]
    async fn refuses_sensitive_path() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        tokio::fs::create_dir(&ssh).await.unwrap();
        let key = ssh.join("id_rsa");
        tokio::fs::write(&key, "FAKE").await.unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = AttachFileTool::new(store);

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
        let tool = AttachFileTool::new(store);
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
        let tool = AttachFileTool::new(store);

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
        let tool = AttachFileTool::new(store);

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
            panic!("expected WithAttachments, got {out:?}");
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
