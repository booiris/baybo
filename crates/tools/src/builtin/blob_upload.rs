//! The seam `AttachFile` and `PutBlob` share: staging a local file into
//! `BlobStore`.
//!
//! Everything up to and including the streaming store write is identical
//! between them — path safety, size and regular-file validation, MIME
//! resolution, the per-call timeout, and the progress/permission surface
//! derived from `path`. Only their *delivery* differs, so only their
//! `execute` bodies do: one returns `ToolOutput::WithAttachments` for the
//! agent loop to hoist onto the final reply, the other a
//! `ToolOutput::Json` reference. Keeping the shared half here is what
//! stops the two from drifting on a check one of them quietly skips.

use std::path::{Path, PathBuf};
use std::time::Duration;

use baybo_model::BlobRef;
use baybo_store::{BlobStore, MAX_BLOB_BYTES};
use futures::StreamExt;
use serde_json::Value;
use tokio_util::io::ReaderStream;

use super::paths::require_absolute;
use crate::{ResourceAccess, ToolError};

pub(super) const MAX_LOCAL_BLOB_BYTES: u64 = MAX_BLOB_BYTES as u64;
pub(super) const MAX_LOCAL_BLOB_MIB: u64 = MAX_LOCAL_BLOB_BYTES / 1024 / 1024;

/// Streams up to 100 MiB into `BlobStore`. On a slow disk the copy alone
/// can take tens of seconds, so the trait-default 30 s is tight; 60 s
/// covers the worst case while still bounding a stuck blob backend.
pub(super) const BLOB_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) struct LocalBlobFile {
    path: PathBuf,
    size: u64,
}

impl LocalBlobFile {
    pub(super) async fn inspect(
        path: &Path,
        tool_name: &str,
        operation: &str,
        max_bytes: u64,
    ) -> crate::Result<Self> {
        require_absolute(path, tool_name, "path")?;

        if baybo_security::is_sensitive_path(path) {
            tracing::warn!(tool = tool_name, path = %path.display(), "blob staging refused sensitive path");
            return Err(ToolError::Execution(format!(
                "refused to {operation} sensitive path {} — credential-bearing files are blocked by security policy",
                path.display()
            )));
        }

        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| ToolError::Execution(format!("stat {}: {e}", path.display())))?;
        if !metadata.is_file() {
            return Err(ToolError::Execution(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let size = metadata.len();
        if size > max_bytes {
            return Err(ToolError::Execution(format!(
                "{} is {size} bytes, exceeds the {max_bytes}-byte cap",
                path.display()
            )));
        }

        Ok(Self {
            path: path.to_path_buf(),
            size,
        })
    }

    pub(super) fn size(&self) -> u64 {
        self.size
    }

    pub(super) async fn put(
        &self,
        blob_store: &dyn BlobStore,
        mime_type: &str,
        max_bytes: u64,
    ) -> crate::Result<BlobRef> {
        let file = tokio::fs::File::open(&self.path)
            .await
            .map_err(|e| ToolError::Execution(format!("open {}: {e}", self.path.display())))?;
        let stream = ReaderStream::new(file).boxed();
        blob_store
            .put_stream(stream, mime_type, None, max_bytes)
            .await
            .map_err(|e| ToolError::Execution(format!("blob upload: {e}")))
    }
}

/// The MIME both tools stage under: an explicit override when the caller
/// supplied one, otherwise the extension guess.
///
/// Validated rather than taken verbatim, because the value outlives the
/// call twice over — it is persisted on the blob row and copied onto the
/// `ContentBlock` clients bucket media by. An empty or newline-bearing
/// override reaches `HeaderValue::from_str` in the gateway's blob
/// download, which refuses it and serves the bytes with **no**
/// `Content-Type` at all. An empty override is the strict-schema spelling of
/// "unset"; newline-bearing values still fail here rather than degrading
/// later at download time.
pub(super) fn resolve_mime_type(requested: Option<String>, path: &Path) -> crate::Result<String> {
    let Some(requested) = requested.filter(|value| !value.trim().is_empty()) else {
        return Ok(guess_mime(path).to_string());
    };
    let mime_type = requested.trim();
    if mime_type.contains(['\r', '\n']) {
        return Err(ToolError::InvalidParams(
            "mime_type must be a single-line value".to_string(),
        ));
    }
    Ok(mime_type.to_string())
}

/// Progress preview and permission surface for a blob tool, both derived
/// from the one `path` argument that identifies the call.
pub(super) fn path_progress_label(params: &Value) -> Option<String> {
    params
        .get("path")
        .and_then(Value::as_str)
        .map(|path| crate::progress::preview_path(Path::new(path)))
}

pub(super) fn path_read_access(params: &Value) -> Vec<ResourceAccess> {
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

pub(super) fn guess_mime(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    match extension.as_deref() {
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
