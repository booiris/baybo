//! In-memory store implementations for downstream crates' tests.
//!
//! Gated behind the `test-support` cargo feature so they never ship in
//! release builds. Add new fakes here as the trait surface grows; keep
//! each fake colocated with the trait it implements (in this crate's
//! sibling modules) so changing the trait forces an update.

use std::collections::HashMap;
use std::io::Cursor;

use async_trait::async_trait;
use baybo_model::BlobRef;
use futures::StreamExt;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use baybo_store::StorageError;
use baybo_store::blob::{
    BlobMeta, BlobReader, BlobStore, ByteStream, Result as BlobResult, SHA256_PREFIX,
};

/// In-memory `BlobStore` for tests. Bytes live in a `Mutex<HashMap>`,
/// keyed by the same `sha256:<hex>` blob id the libsql backend uses, so
/// downstream tests can swap between fakes and real stores without
/// changing assertion strings.
#[derive(Debug, Default)]
pub struct MemoryBlobStore {
    blobs: Mutex<HashMap<String, MemoryBlob>>,
}

#[derive(Debug, Clone)]
struct MemoryBlob {
    bytes: Vec<u8>,
    mime_type: String,
    created_at: i64,
    last_accessed_at: i64,
    read_token: String,
}

impl MemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.blobs.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[async_trait]
impl BlobStore for MemoryBlobStore {
    async fn put(
        &self,
        bytes: &[u8],
        mime_type: &str,
        _uploader_identity: Option<&str>,
    ) -> BlobResult<BlobRef> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hex = hex_encode(&hasher.finalize());
        let read_token = "fake-token";
        let blob_id = format!("{SHA256_PREFIX}{hex}.{read_token}");
        let now = chrono::Utc::now().timestamp_micros();
        let mut guard = self.blobs.lock();
        guard
            .entry(blob_id.clone())
            .and_modify(|b| {
                b.mime_type = mime_type.to_owned();
                b.last_accessed_at = now;
            })
            .or_insert(MemoryBlob {
                bytes: bytes.to_vec(),
                mime_type: mime_type.to_owned(),
                created_at: now,
                last_accessed_at: now,
                read_token: read_token.to_owned(),
            });
        Ok(BlobRef { blob_id })
    }

    async fn put_stream(
        &self,
        mut stream: ByteStream,
        mime_type: &str,
        uploader_identity: Option<&str>,
        max_bytes: u64,
    ) -> BlobResult<BlobRef> {
        // Tests only — buffer everything into a Vec and reuse the
        // single-shot `put` path. The real LibsqlBlobStore streams to
        // disk; the fake stays in memory.
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| StorageError::Internal(anyhow::anyhow!("stream io: {e}")))?;
            if (buf.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
                return Err(StorageError::TooLarge {
                    limit: max_bytes,
                    actual: (buf.len() + chunk.len()) as u64,
                });
            }
            buf.extend_from_slice(&chunk);
        }
        self.put(&buf, mime_type, uploader_identity).await
    }

    async fn get(&self, blob_id: &str) -> BlobResult<Vec<u8>> {
        // Funnel through `stat` so the LRU touch + token check fire on
        // every read, matching the libsql backend's contract.
        let _ = self.stat(blob_id).await?;
        match self.blobs.lock().get(blob_id) {
            Some(b) => Ok(b.bytes.clone()),
            None => Err(StorageError::NotFound(format!("blob {blob_id}"))),
        }
    }

    async fn open(&self, blob_id: &str) -> BlobResult<BlobReader> {
        let bytes = self.get(blob_id).await?;
        Ok(Box::pin(Cursor::new(bytes)))
    }

    async fn stat(&self, blob_id: &str) -> BlobResult<BlobMeta> {
        let (_hex, token) = split_id(blob_id)?;
        let now = chrono::Utc::now().timestamp_micros();
        let mut guard = self.blobs.lock();
        match guard.get_mut(blob_id) {
            Some(b) => {
                if b.read_token != token {
                    return Err(StorageError::NotFound(format!("blob {blob_id}")));
                }
                // LRU touch — mirror the libsql backend so tests that
                // exercise touch-on-access behave identically against
                // either store.
                b.last_accessed_at = now;
                Ok(BlobMeta {
                    blob_id: blob_id.to_owned(),
                    mime_type: b.mime_type.clone(),
                    size: b.bytes.len() as u64,
                    created_at: b.created_at,
                    read_token: Some(b.read_token.clone()),
                })
            }
            None => Err(StorageError::NotFound(format!("blob {blob_id}"))),
        }
    }

    async fn delete(&self, blob_id: &str) -> BlobResult<()> {
        let _ = split_id(blob_id)?;
        self.blobs.lock().remove(blob_id);
        Ok(())
    }

    async fn purge_older_than(&self, cutoff_unix: i64) -> BlobResult<u64> {
        let mut guard = self.blobs.lock();
        let before = guard.len();
        guard.retain(|_, blob| blob.last_accessed_at >= cutoff_unix);
        Ok((before - guard.len()) as u64)
    }
}

fn split_id(blob_id: &str) -> BlobResult<(&str, &str)> {
    let hex_all = blob_id
        .strip_prefix(SHA256_PREFIX)
        .ok_or_else(|| StorageError::NotFound(format!("invalid blob_id {blob_id}")))?;
    let (hex, token) = hex_all.split_once('.').unwrap_or((hex_all, ""));
    Ok((hex, token))
}
