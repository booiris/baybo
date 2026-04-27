use std::pin::Pin;

use async_trait::async_trait;
use aura_model::BlobRef;
use bytes::Bytes;
use futures::stream::BoxStream;
use tokio::io::AsyncRead;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Stream of byte chunks for [`BlobStore::put_stream`]. Each chunk
/// surfaces a transport `io::Error` independently so a torn HTTP body
/// drops the in-flight blob without reaching the filesystem.
pub type ByteStream = BoxStream<'static, std::io::Result<Bytes>>;

/// Boxed `AsyncRead` returned by [`BlobStore::open`]. Pinned so axum's
/// `tokio_util::io::ReaderStream` can wrap it directly into an HTTP
/// body without buffering the file.
pub type BlobReader = Pin<Box<dyn AsyncRead + Send>>;

/// Algorithm prefix used in every minted `BlobRef::blob_id`. The full id
/// is `"sha256:<64 lower-hex>"`, so callers can recognize a content-
/// addressed blob from a glance and the prefix gives us room to add
/// other digests later without breaking parsers.
pub const SHA256_PREFIX: &str = "sha256:";

/// Metadata for a stored blob. Returned by [`BlobStore::stat`].
///
/// `size` and `created_at` come from the libsql metadata row; the actual
/// bytes live on disk under `<blobs_root>/<sha256[0..2]>/<sha256>` so a
/// libsql query never has to read the file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    pub blob_id: String,
    pub mime_type: String,
    pub size: u64,
    /// Unix seconds at which the blob was first written (or revived
    /// after a soft delete).
    pub created_at: i64,
    /// Unguessable token required to read the blob via the gateway's
    /// side-channel. Prevents ID prediction across tenant boundaries.
    pub read_token: Option<String>,
}

#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Persist `bytes` and return a fresh [`BlobRef`]. Each call mints
    /// a new unguessable `read_token` and returns a distinct
    /// `blob_id` — even for identical content. The on-disk byte file
    /// is content-addressed by the SHA-256 digest, so duplicate
    /// content shares the same physical file (deduplication at the
    /// filesystem layer); only the metadata row + capability id are
    /// per-call.
    ///
    /// Two consequences for callers:
    /// * The id is the read capability. Anyone who learns it can
    ///   read; sharing a `blob_id` means delegating read access.
    /// * "Same bytes" is not the same as "same id". A test relying
    ///   on dedup-by-id from `put` is wrong; use the bytes you
    ///   uploaded as the comparison anchor.
    async fn put(
        &self,
        bytes: &[u8],
        mime_type: &str,
        uploader_identity: Option<&str>,
    ) -> Result<BlobRef>;

    /// Streaming variant of [`put`]. Pulls chunks from `stream`, hashes
    /// them on the fly, and writes to a per-attempt temp file before
    /// renaming to the content-addressed final path. `max_bytes` is
    /// enforced incrementally — the first chunk that pushes the running
    /// total past the cap returns [`StorageError::TooLarge`] without
    /// buffering the rest. Pass `u64::MAX` to opt out.
    ///
    /// Same per-call distinct-id semantics as `put`.
    async fn put_stream(
        &self,
        stream: ByteStream,
        mime_type: &str,
        uploader_identity: Option<&str>,
        max_bytes: u64,
    ) -> Result<BlobRef>;

    /// Read the full blob bytes. Returns [`StorageError::NotFound`]
    /// when no live row exists for `blob_id` (missing or soft-deleted).
    async fn get(&self, blob_id: &str) -> Result<Vec<u8>>;

    /// Open a streaming reader for the blob's bytes. The caller owns
    /// the returned [`AsyncRead`] until done. Same not-found semantics
    /// as `get`. Used by the gateway's `GET /v1/blobs/{id}` to avoid
    /// loading large blobs into memory before responding.
    async fn open(&self, blob_id: &str) -> Result<BlobReader>;

    /// Return metadata only. Same not-found semantics as `get`.
    async fn stat(&self, blob_id: &str) -> Result<BlobMeta>;

    /// Soft-delete: marks the metadata row's `deleted_at`. The on-disk
    /// bytes stay so a future `put` of the same content can revive the
    /// row in O(1). Idempotent on missing / already-deleted ids.
    async fn delete(&self, blob_id: &str) -> Result<()>;

    /// Bulk garbage-collect every live blob whose `created_at <
    /// cutoff_unix`. Soft-deletes the metadata row and (where the
    /// backend keeps payload files on disk) unlinks the byte file iff
    /// no remaining live row resolves to the same on-disk path. Returns
    /// the number of rows transitioned from live to deleted.
    /// Idempotent: a no-op when no live row matches.
    async fn purge_older_than(&self, cutoff_unix: i64) -> Result<u64>;
}
