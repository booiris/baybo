use std::pin::Pin;

use async_trait::async_trait;
use baybo_model::BlobRef;
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

/// Algorithm prefix on every minted `BlobRef::blob_id`. Re-exported from
/// [`baybo_model`] (its single source of truth, next to `BlobRef`) so existing
/// `baybo_store::SHA256_PREFIX` / `baybo_store::blob::SHA256_PREFIX` callers are
/// unchanged.
pub use baybo_model::SHA256_PREFIX;

/// Metadata for a stored blob. Returned by [`BlobStore::stat`].
///
/// `size` and `created_at` come from the sqlite metadata row; the actual
/// bytes live on disk under `<blobs_root>/<sha256[0..2]>/<sha256>` so a
/// sqlite query never has to read the file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    pub blob_id: String,
    pub mime_type: String,
    pub size: u64,
    /// Unix microseconds at which the blob was first written.
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
    /// when no row exists for `blob_id`.
    async fn get(&self, blob_id: &str) -> Result<Vec<u8>>;

    /// Open a streaming reader for the blob's bytes. The caller owns
    /// the returned [`AsyncRead`] until done. Same not-found semantics
    /// as `get`. Used by the gateway's `GET /v1/blobs/{id}` to avoid
    /// loading large blobs into memory before responding.
    async fn open(&self, blob_id: &str) -> Result<BlobReader>;

    /// Open a streaming reader beginning at byte `offset` (resumable
    /// downloads — e.g. the relay blob leg's `BlobPull{offset}`).
    ///
    /// **Token gate (security-critical):** the offset path MUST enforce
    /// the same `read_token` check as [`open`]/[`stat`] — never resolve a
    /// caller-supplied id by its bare hex digest. The default upholds this
    /// by going through [`open`] (which `stat`s first) and discarding
    /// `offset` bytes; a backend with seekable storage should override to
    /// seek natively, but only after re-`stat`ing. `offset` past the
    /// blob's length yields an empty reader.
    async fn open_at(&self, blob_id: &str, offset: u64) -> Result<BlobReader> {
        let mut reader = self.open(blob_id).await?;
        // `open` already enforced the token gate; skip to `offset`.
        let mut remaining = offset;
        if remaining > 0 {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 64 * 1024];
            while remaining > 0 {
                let want = remaining.min(buf.len() as u64) as usize;
                let n = reader.read(&mut buf[..want]).await.map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("blob skip to offset: {e}"))
                })?;
                if n == 0 {
                    break; // offset past EOF → an empty reader
                }
                remaining -= n as u64;
            }
        }
        Ok(reader)
    }

    /// Return metadata only. Same not-found semantics as `get`.
    async fn stat(&self, blob_id: &str) -> Result<BlobMeta>;

    /// Hard-delete the metadata row and unlink the on-disk payload
    /// (when no other live row resolves to the same content path).
    /// Idempotent on missing ids.
    async fn delete(&self, blob_id: &str) -> Result<()>;
}
