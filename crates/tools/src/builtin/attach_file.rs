//! `AttachFile` — stream a local file into the blob store and attach it to
//! the turn's final assistant reply.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use baybo_model::{BlobRef, ContentBlock, TrustLevel};
use baybo_store::BlobStore;
use serde::Deserialize;
use serde_json::{Value, json};

use super::blob_upload::{LocalBlobFile, MAX_LOCAL_BLOB_BYTES, guess_mime};
use crate::{
    ResourceAccess, Tool, ToolCapability, ToolContext, ToolError, ToolManifest, ToolOutput,
};

const TOOL_NAME: &str = "AttachFile";
const MAX_BYTES: u64 = MAX_LOCAL_BLOB_BYTES;
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

        let source = LocalBlobFile::inspect(&p.path, TOOL_NAME, "attach", MAX_BYTES).await?;
        let size = source.size();

        let filename = p.filename.unwrap_or_else(|| {
            p.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string())
        });
        let mime = p.mime.unwrap_or_else(|| guess_mime(&p.path).to_string());

        let duration_ms = probe_media_duration_ms(&p.path, &mime).await;
        let page_count = probe_pdf_page_count(&p.path, &mime, size).await;
        let dimensions = probe_image_dimensions(&p.path, &mime, size).await;

        let blob_ref = source
            .put(self.blob_store.as_ref(), &mime, MAX_BYTES)
            .await?;

        // The agent loop hoists these attachments onto the turn's final
        // assistant message, so the file persists with the transcript and
        // survives a reload. Nothing has reached the user yet — say so, or
        // the model reports a delivery that a cancelled turn never makes.
        Ok(ToolOutput::WithAttachments {
            text: format!("Attached {filename} ({size} bytes, {mime}) to your final reply."),
            attachments: vec![media_block(
                blob_ref,
                filename,
                mime,
                ProbedMedia {
                    duration_ms,
                    page_count,
                    size_bytes: u32::try_from(size).ok(),
                    width: dimensions.map(|(w, _)| w),
                    height: dimensions.map(|(_, h)| h),
                },
            )],
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
        let ms = if mime == "audio/mpeg" {
            // MP3 first: header math (first-frame bitrate × size) lies for VBR
            // files without a correct Xing header — both lofty here and
            // AVPlayer's default estimate on the device get it wrong, and the
            // card's number must match what actually plays. A frame walk is
            // the only honest count; fall back to the estimate if the walk
            // chokes on a malformed tail.
            mp3_duration_ms(&path).or_else(|| audio_duration_ms(&path))
        } else if mime.starts_with("audio/") {
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

/// A PDF's page count, read at the same moment and for the same reason as
/// a track's duration: the provider bills a native document per PAGE, so
/// this is what the context budget charges. Only the ingester ever sees
/// the bytes, and the block that outlives them has to carry the number.
async fn probe_pdf_page_count(path: &Path, mime: &str, size: u64) -> Option<u32> {
    // The same predicate the delivery path and the budget classify by, so
    // the three can never disagree about what counts as a PDF.
    if !baybo_llm::delivers_pdf_document(mime) {
        return None;
    }
    probe_payload(
        path,
        size,
        baybo_llm::MAX_PDF_DOCUMENT_BYTES as u64,
        baybo_llm::media_probe::pdf_page_count,
    )
    .await
}

/// An image's pixel dimensions, probed here for the same reason: a
/// provider cuts an image into tiles of a fixed PIXEL size and bills per
/// tile, so this is the price and the file's byte count is not.
async fn probe_image_dimensions(path: &Path, mime: &str, size: u64) -> Option<(u32, u32)> {
    if !mime.starts_with("image/") {
        return None;
    }
    probe_payload(
        path,
        size,
        baybo_llm::MAX_IMAGE_DOCUMENT_BYTES as u64,
        baybo_llm::media_probe::image_dimensions,
    )
    .await
}

/// Read a payload and answer a question about it, refusing anything the
/// delivery path would stub on size — above that cap the block never
/// reaches a provider, so a fact recovered from it is a price charged for
/// something that costs the text stub. `spawn_blocking` because both
/// parsers are sync IO over the whole payload.
async fn probe_payload<T, F>(path: &Path, size: u64, max_bytes: u64, probe: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&[u8]) -> Option<T> + Send + 'static,
{
    if size > max_bytes {
        return None;
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || probe(&std::fs::read(&path).ok()?))
        .await
        .ok()
        .flatten()
}

fn mp3_duration_ms(path: &Path) -> Option<u32> {
    let duration = mp3_duration::from_path(path).ok()?;
    u32::try_from(duration.as_millis()).ok()
}

fn audio_duration_ms(path: &Path) -> Option<u32> {
    use lofty::file::AudioFile;
    use lofty::probe::Probe;
    // Sniff the container by CONTENT, not extension: an Opus stream inside a
    // `.ogg` fails the extension guess (lofty assumes Vorbis there) and would
    // ship without a duration. Measured on synthetic 240s files: the
    // extension path errors on Opus-in-.ogg; the sniff reads both Vorbis and
    // Opus exactly.
    let file = Probe::open(path)
        .ok()?
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;
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
    probed: ProbedMedia,
) -> ContentBlock {
    if mime_type.starts_with("image/") {
        ContentBlock::Image {
            blob,
            mime_type,
            filename: Some(filename),
            width: probed.width,
            height: probed.height,
        }
    } else if mime_type.starts_with("audio/") {
        ContentBlock::Audio {
            blob,
            mime_type,
            filename: Some(filename),
            duration_ms: probed.duration_ms,
        }
    } else {
        ContentBlock::File {
            blob,
            filename,
            mime_type,
            duration_ms: probed.duration_ms,
            page_count: probed.page_count,
            size_bytes: probed.size_bytes,
        }
    }
}

/// What the ingest probes recovered from the bytes. Every field is a
/// price input the context budget spends, and all five are `Option<u32>`
/// — a struct so the call site names them instead of lining five
/// interchangeable arguments up positionally.
struct ProbedMedia {
    duration_ms: Option<u32>,
    page_count: Option<u32>,
    size_bytes: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
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
        channels: Vec::new(),
    };
    (Arc::new(attach), manifest)
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

    /// Attach time is the only moment the bytes are in hand, and the
    /// `ContentBlock` that outlives them is all the context budget ever
    /// sees. A provider bills a native PDF per PAGE, so the count is
    /// probed here for the same reason a track's duration already was.
    #[tokio::test]
    async fn an_attached_pdf_carries_its_probed_page_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = AttachFileTool::new(store);

        for pages in [1usize, 13, 200] {
            let p = dir.path().join(format!("doc{pages}.pdf"));
            tokio::fs::write(&p, baybo_llm::media_probe::fixture::object_stream(pages))
                .await
                .unwrap();
            let out = tool.execute(json!({ "path": p }), &ctx()).await.unwrap();
            let ToolOutput::WithAttachments { attachments, .. } = out else {
                panic!("expected WithAttachments");
            };
            match &attachments[0] {
                ContentBlock::File { page_count, .. } => {
                    assert_eq!(*page_count, Some(pages as u32))
                }
                other => panic!("expected File block, got {other:?}"),
            }
        }

        // Not a PDF, or not parseable: no number rather than a wrong one.
        for (name, bytes) in [
            ("bundle.zip", b"PK\x03\x04".as_slice()),
            ("broken.pdf", b"%PDF-1.4 but not really".as_slice()),
        ] {
            let p = dir.path().join(name);
            tokio::fs::write(&p, bytes).await.unwrap();
            let out = tool.execute(json!({ "path": p }), &ctx()).await.unwrap();
            let ToolOutput::WithAttachments { attachments, .. } = out else {
                panic!("expected WithAttachments for {name}");
            };
            match &attachments[0] {
                ContentBlock::File { page_count, .. } => assert_eq!(*page_count, None, "{name}"),
                other => panic!("expected File block, got {other:?}"),
            }
        }
    }

    /// A text-like attachment reaches the model as inlined prompt text, so
    /// its byte count is its price. Without it the budget charged the full
    /// delivery cap — 17,529 tokens for a file of a few hundred bytes.
    #[tokio::test]
    async fn an_attached_file_carries_its_byte_size() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = AttachFileTool::new(store);

        let body = "# notes\nnothing much here\n";
        let p = dir.path().join("notes.md");
        tokio::fs::write(&p, body).await.unwrap();
        let out = tool.execute(json!({ "path": p }), &ctx()).await.unwrap();
        let ToolOutput::WithAttachments { attachments, .. } = out else {
            panic!("expected WithAttachments");
        };
        match &attachments[0] {
            ContentBlock::File { size_bytes, .. } => {
                assert_eq!(*size_bytes, Some(body.len() as u32))
            }
            other => panic!("expected File block, got {other:?}"),
        }
    }

    /// An image is billed per tile of its PIXEL grid, so the dimensions
    /// are its price and the file's byte count is not — a 12000x9000 flat
    /// render is under a megabyte and costs 49,536 tokens.
    #[tokio::test]
    async fn an_attached_image_carries_its_probed_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = AttachFileTool::new(store);

        for (w, h) in [(3024u32, 4032u32), (12000, 9000)] {
            let p = dir.path().join(format!("shot{w}x{h}.png"));
            tokio::fs::write(&p, baybo_llm::media_probe::fixture::png(w, h))
                .await
                .unwrap();
            let out = tool.execute(json!({ "path": p }), &ctx()).await.unwrap();
            let ToolOutput::WithAttachments { attachments, .. } = out else {
                panic!("expected WithAttachments");
            };
            match &attachments[0] {
                ContentBlock::Image { width, height, .. } => {
                    assert_eq!((*width, *height), (Some(w), Some(h)))
                }
                other => panic!("expected Image block, got {other:?}"),
            }
        }

        // A vector image has no pixel grid to read, so it carries none.
        let p = dir.path().join("logo.svg");
        tokio::fs::write(&p, br#"<svg width="99999" height="99999"/>"#)
            .await
            .unwrap();
        let out = tool.execute(json!({ "path": p }), &ctx()).await.unwrap();
        let ToolOutput::WithAttachments { attachments, .. } = out else {
            panic!("expected WithAttachments");
        };
        match &attachments[0] {
            ContentBlock::Image { width, height, .. } => {
                assert_eq!((*width, *height), (None, None))
            }
            other => panic!("expected Image block, got {other:?}"),
        }
    }

    /// `AttachFile` takes files up to 100 MiB, but the LLM layer stubs a
    /// PDF over 8 MiB — so probing past that cap buys a page price for a
    /// block that is always delivered as a text stub, and reads the whole
    /// payload into memory to do it.
    #[tokio::test]
    async fn a_payload_over_its_own_delivery_cap_is_not_probed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        let tool = AttachFileTool::new(store);

        // Padded in FRONT of the header, which every lopdf entry point
        // skips, so the document still parses at any size.
        let padded = |bytes: usize| {
            let doc = baybo_llm::media_probe::fixture::classic(3);
            let mut out = vec![b'\n'; bytes - doc.len()];
            out.extend_from_slice(&doc);
            out
        };
        for (bytes, expected) in [
            (baybo_llm::MAX_PDF_DOCUMENT_BYTES, Some(3)),
            (baybo_llm::MAX_PDF_DOCUMENT_BYTES + 1, None),
        ] {
            let payload = padded(bytes);
            assert_eq!(
                baybo_llm::media_probe::pdf_page_count(&payload),
                Some(3),
                "{bytes}: the probe itself must still be able to read this"
            );
            let p = dir.path().join(format!("doc{bytes}.pdf"));
            tokio::fs::write(&p, &payload).await.unwrap();
            let out = tool.execute(json!({ "path": p }), &ctx()).await.unwrap();
            let ToolOutput::WithAttachments { attachments, .. } = out else {
                panic!("expected WithAttachments");
            };
            match &attachments[0] {
                ContentBlock::File { page_count, .. } => {
                    assert_eq!(*page_count, expected, "{bytes}")
                }
                other => panic!("expected File block, got {other:?}"),
            }
        }
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

    /// The duration probe must not trust headers or extensions — both lied in
    /// the field (a 4:00 ogg showed 3:41 at rest and 5:04 playing). The 2s
    /// fixtures pin the two failure classes: a VBR MP3 without a Xing header
    /// (first-frame bitrate math reported ~6× off; the frame walk is honest)
    /// and an Opus stream inside `.ogg` (lofty's extension guess assumes
    /// Vorbis and errors; the content sniff reads it exactly).
    #[tokio::test]
    async fn duration_probe_survives_vbr_mp3_and_opus_in_ogg() {
        let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/audio");
        let cases = [
            ("vbr-noxing-2s.mp3", "audio/mpeg"),
            ("opus-2s.ogg", "audio/ogg"),
            ("vorbis-2s.ogg", "audio/ogg"),
        ];
        for (file, mime) in cases {
            let path = Path::new(fixtures).join(file);
            let ms = probe_media_duration_ms(&path, mime)
                .await
                .unwrap_or_else(|| panic!("{file}: no duration"));
            // Encoders pad a trailing partial frame; ±25% still catches both
            // regressions (the estimate was 600% off, the parse a hard error).
            assert!(
                (1500..=2500).contains(&ms),
                "{file}: got {ms}ms for a 2s fixture"
            );
        }
    }
}
