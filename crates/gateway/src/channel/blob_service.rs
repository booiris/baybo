//! Shared blob side-channel operations.
//!
//! HTTP routes and relay API tunnel legs have different transports, but their
//! blob semantics are the same: capability-id download, optional range resume,
//! capped streaming upload, and optional content-hash verification.

use axum::http::StatusCode;
use baybo_model::BlobRef;
use baybo_store::{BlobReader, BlobStore, ByteStream, StorageError, blob::SHA256_PREFIX};

pub(crate) const MAX_BLOB_BYTES: usize = 100 * 1024 * 1024;
pub(crate) const DEFAULT_BLOB_MIME: &str = "application/octet-stream";
pub(crate) const HEADER_CONTENT_SHA256: &str = "x-baybo-content-sha256";

pub(crate) struct BlobDownload {
    pub(crate) status: StatusCode,
    pub(crate) mime_type: String,
    pub(crate) body_len: u64,
    pub(crate) content_range: Option<String>,
    pub(crate) reader: BlobReader,
}

#[derive(Debug)]
pub(crate) enum BlobServiceError {
    BadRequest(&'static str),
    NotFound,
    RangeNotSatisfiable(&'static str),
    PayloadTooLarge { limit: u64 },
    ContentHashMismatch,
    StoreFailure,
}

impl BlobServiceError {
    pub(crate) fn status_code(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) | Self::ContentHashMismatch => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::RangeNotSatisfiable(_) => StatusCode::RANGE_NOT_SATISFIABLE,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::StoreFailure => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub(crate) fn client_message(&self) -> String {
        match self {
            Self::BadRequest(reason) | Self::RangeNotSatisfiable(reason) => reason.to_string(),
            Self::NotFound => "blob not found".to_string(),
            Self::PayloadTooLarge { limit } => format!("blob exceeds {limit}-byte cap"),
            Self::ContentHashMismatch => "content hash mismatch".to_string(),
            Self::StoreFailure => "blob store failure".to_string(),
        }
    }
}

pub(crate) fn parse_range_start(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|rest| {
            rest.split_once('-')
                .map_or(rest, |(start, _)| start)
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

pub(crate) fn require_sha256_hex(value: Option<&str>) -> Result<String, BlobServiceError> {
    match value {
        Some(value) if is_sha256_hex(value) => Ok(value.to_owned()),
        _ => Err(BlobServiceError::BadRequest("missing content hash")),
    }
}

pub(crate) async fn open_download(
    blob_store: &dyn BlobStore,
    blob_id: &str,
    offset: u64,
) -> Result<BlobDownload, BlobServiceError> {
    let meta = match blob_store.stat(blob_id).await {
        Ok(meta) => meta,
        Err(StorageError::NotFound(_)) => return Err(BlobServiceError::NotFound),
        Err(e) => {
            tracing::error!(error = %e, blob_id, "blob stat failed");
            return Err(BlobServiceError::StoreFailure);
        }
    };

    if offset > meta.size {
        return Err(BlobServiceError::RangeNotSatisfiable(
            "range starts past end of blob",
        ));
    }

    let reader = match blob_store.open_at(blob_id, offset).await {
        Ok(reader) => reader,
        Err(StorageError::NotFound(_)) => return Err(BlobServiceError::NotFound),
        Err(e) => {
            tracing::error!(error = %e, blob_id, "blob open failed");
            return Err(BlobServiceError::StoreFailure);
        }
    };

    let body_len = meta.size - offset;
    let status = if offset > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let content_range = (offset > 0).then(|| {
        format!(
            "bytes {offset}-{}/{}",
            meta.size.saturating_sub(1),
            meta.size
        )
    });

    Ok(BlobDownload {
        status,
        mime_type: meta.mime_type,
        body_len,
        content_range,
        reader,
    })
}

pub(crate) async fn put_upload(
    blob_store: &dyn BlobStore,
    stream: ByteStream,
    mime_type: &str,
    uploader_identity: Option<&str>,
    claimed_sha256: Option<&str>,
) -> Result<BlobRef, BlobServiceError> {
    let blob_ref = match blob_store
        .put_stream(stream, mime_type, uploader_identity, MAX_BLOB_BYTES as u64)
        .await
    {
        Ok(blob_ref) => blob_ref,
        Err(StorageError::TooLarge { limit, actual }) => {
            tracing::debug!(limit, actual, "blob upload exceeded cap");
            return Err(BlobServiceError::PayloadTooLarge { limit });
        }
        Err(e) => {
            tracing::error!(error = %e, "blob upload persist failed");
            return Err(BlobServiceError::StoreFailure);
        }
    };

    if let Some(claimed) = claimed_sha256 {
        let got = blob_ref
            .blob_id
            .strip_prefix(SHA256_PREFIX)
            .and_then(|rest| rest.split('.').next())
            .unwrap_or_default();
        if got != claimed {
            if let Err(e) = blob_store.delete(&blob_ref.blob_id).await {
                tracing::warn!(error = %e, "failed to delete hash-mismatched blob");
            }
            return Err(BlobServiceError::ContentHashMismatch);
        }
    }

    Ok(blob_ref)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
