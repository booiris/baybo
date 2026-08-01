use std::path::{Path, PathBuf};

use baybo_model::BlobRef;
use baybo_store::{BlobStore, MAX_BLOB_BYTES};
use futures::StreamExt;
use tokio_util::io::ReaderStream;

use super::paths::require_absolute;
use crate::ToolError;

pub(super) const MAX_LOCAL_BLOB_BYTES: u64 = MAX_BLOB_BYTES as u64;

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
