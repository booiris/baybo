use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use aura_model::BlobRef;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::LibsqlPool;
use crate::StorageError;
use crate::blob::{BlobMeta, BlobReader, BlobStore, ByteStream, Result, SHA256_PREFIX};

// `DirBuilder::mode(0o700)` and `OpenOptions::mode(0o600)` lock down
// every directory and file in the blob tree to the owning UID — the
// channel-token boundary is the only gate on user-uploaded media, and
// world-readable bytes on disk would bypass it. Aura is Unix-only (see
// `CLAUDE.md`) so neither needs a cfg gate.

pub struct LibsqlBlobStore {
    pool: LibsqlPool,
    root: PathBuf,
}

impl LibsqlBlobStore {
    /// Construct a blob store backed by `pool` for metadata and `root`
    /// on the filesystem for byte payloads. The caller is responsible
    /// for creating `root` (or using [`Self::open`] which does it for
    /// you).
    pub fn new(pool: LibsqlPool, root: impl Into<PathBuf>) -> Self {
        Self {
            pool,
            root: root.into(),
        }
    }

    /// Construct a blob store and ensure `root` exists on disk.
    pub async fn open(pool: LibsqlPool, root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .map_err(|e| anyhow::anyhow!("failed to create blob root {}: {e}", root.display()))?;
        Ok(Self { pool, root })
    }

    fn blob_path(&self, hex: &str) -> PathBuf {
        // 2-char shard prefix to keep one directory from accumulating
        // every blob — same shape git uses for its objects.
        self.root.join(&hex[..2]).join(hex)
    }

    /// Top-level scratch directory for in-flight streaming uploads.
    /// Lives under the blob root with a leading dot so the 2-char hex
    /// shard scan can never collide. Contents are unique-per-attempt
    /// and reaped on success (rename) or error (explicit cleanup).
    fn scratch_dir(&self) -> PathBuf {
        self.root.join(".tmp")
    }

    /// Persist the metadata row + return the `BlobRef`. Idempotent:
    /// `ON CONFLICT … DO UPDATE` keeps the latest mime and clears
    /// `deleted_at`, matching the soft-delete revival rule.
    async fn record_metadata(
        &self,
        hex: &str,
        mime_type: &str,
        size: u64,
        uploader_identity: Option<&str>,
        read_token: &str,
    ) -> Result<BlobRef> {
        let blob_id = format!("{SHA256_PREFIX}{hex}.{read_token}");
        let conn = self.pool.conn();
        conn.execute(
            "INSERT INTO blobs (blob_id, mime_type, size, uploader_identity, read_token, created_at, deleted_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL) \
             ON CONFLICT(blob_id) DO UPDATE SET \
                mime_type = excluded.mime_type, \
                deleted_at = NULL",
            libsql::params![
                blob_id.clone(),
                mime_type.to_string(),
                size as i64,
                uploader_identity.map(|s| s.to_string()),
                read_token.to_string(),
                now_unix(),
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql blob upsert: {e}")))?;
        Ok(BlobRef { blob_id })
    }
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[async_trait]
impl BlobStore for LibsqlBlobStore {
    async fn put(
        &self,
        bytes: &[u8],
        mime_type: &str,
        uploader_identity: Option<&str>,
    ) -> Result<BlobRef> {
        // Single-call put: wrap the buffer as a one-chunk stream so
        // every code path goes through the same write+rename pipeline.
        // `Bytes::copy_from_slice` is unavoidable here — the trait
        // takes a borrowed slice but the stream's chunks must be
        // `'static`-owned for the spawned write loop.
        let chunk = Bytes::copy_from_slice(bytes);
        let stream = stream::once(async move { Ok::<_, std::io::Error>(chunk) }).boxed();
        self.put_stream(stream, mime_type, uploader_identity, u64::MAX)
            .await
    }

    async fn put_stream(
        &self,
        mut stream: ByteStream,
        mime_type: &str,
        uploader_identity: Option<&str>,
        max_bytes: u64,
    ) -> Result<BlobRef> {
        // Hash + size are unknown until the stream drains, so we can't
        // pick the final shard path up front. Land everything in a
        // top-level scratch directory first, then rename to
        // `<root>/<hex[..2]>/<hex>` once the digest settles.
        let scratch = self.scratch_dir();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&scratch)
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!(
                    "create scratch dir {}: {e}",
                    scratch.display()
                ))
            })?;
        let tmp = scratch.join(unique_tmp_name());

        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let cleanup = |path: &Path| {
            let p = path.to_path_buf();
            tokio::spawn(async move {
                let _ = tokio::fs::remove_file(p).await;
            });
        };

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .await
            .map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("create blob tmp {}: {e}", tmp.display()))
            })?;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(e) => {
                    cleanup(&tmp);
                    return Err(StorageError::Internal(anyhow::anyhow!(
                        "blob upload stream error: {e}"
                    )));
                }
            };
            let chunk_len = chunk.len() as u64;
            // Cap is incremental — we never see chunk N+1 if N already
            // pushed total over the limit, and we don't write the
            // overflowing chunk so disk usage stays bounded.
            if total.saturating_add(chunk_len) > max_bytes {
                cleanup(&tmp);
                return Err(StorageError::TooLarge {
                    limit: max_bytes,
                    actual: total + chunk_len,
                });
            }
            hasher.update(&chunk);
            if let Err(e) = file.write_all(&chunk).await {
                cleanup(&tmp);
                return Err(StorageError::Internal(anyhow::anyhow!(
                    "write blob tmp {}: {e}",
                    tmp.display()
                )));
            }
            total += chunk_len;
        }
        if let Err(e) = file.sync_all().await {
            cleanup(&tmp);
            return Err(StorageError::Internal(anyhow::anyhow!(
                "fsync blob tmp {}: {e}",
                tmp.display()
            )));
        }
        drop(file);

        let hex = hex_encode(&hasher.finalize());
        let read_token = unique_read_token();
        let final_path = self.blob_path(&hex);
        // Skip the rename when the canonical path already exists —
        // some other put or a previous attempt got there first. Either
        // way the bytes match (content-addressed) so we just clean
        // up our own scratch file and proceed to metadata.
        if path_exists(&final_path).await {
            cleanup(&tmp);
        } else {
            if let Some(parent) = final_path.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)
                    .map_err(|e| {
                        StorageError::Internal(anyhow::anyhow!(
                            "create blob shard {}: {e}",
                            parent.display()
                        ))
                    })?;
            }
            if let Err(e) = tokio::fs::rename(&tmp, &final_path).await {
                cleanup(&tmp);
                return Err(StorageError::Internal(anyhow::anyhow!(
                    "rename blob {} -> {}: {e}",
                    tmp.display(),
                    final_path.display()
                )));
            }
        }

        self.record_metadata(&hex, mime_type, total, uploader_identity, &read_token)
            .await
    }

    async fn get(&self, blob_id: &str) -> Result<Vec<u8>> {
        let (hex, _token) = split_id(blob_id)?;
        // stat first so a soft-deleted blob whose bytes are still on
        // disk surfaces as NotFound rather than reading them anyway.
        let _ = self.stat(blob_id).await?;
        let path = self.blob_path(hex);
        tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(format!("blob bytes missing for {blob_id}"))
            } else {
                StorageError::Internal(anyhow::anyhow!("read blob {}: {e}", path.display()))
            }
        })
    }

    async fn open(&self, blob_id: &str) -> Result<BlobReader> {
        let (hex, _token) = split_id(blob_id)?;
        let _ = self.stat(blob_id).await?;
        let path = self.blob_path(hex);
        let file = tokio::fs::File::open(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(format!("blob bytes missing for {blob_id}"))
            } else {
                StorageError::Internal(anyhow::anyhow!("open blob {}: {e}", path.display()))
            }
        })?;
        Ok(Box::pin(file))
    }

    async fn stat(&self, blob_id: &str) -> Result<BlobMeta> {
        let (_hex, token) = split_id(blob_id)?;
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT blob_id, mime_type, size, created_at, read_token \
                 FROM blobs WHERE blob_id = ?1 AND deleted_at IS NULL",
                libsql::params![blob_id.to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql blob query: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql blob row: {e}")))?
            .ok_or_else(|| StorageError::NotFound(format!("blob {blob_id}")))?;

        let id: String = row
            .get(0)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get blob_id: {e}")))?;
        let mime_type: String = row
            .get(1)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get mime: {e}")))?;
        let size: i64 = row
            .get(2)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get size: {e}")))?;
        let created_at: i64 = row
            .get(3)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get created_at: {e}")))?;
        let read_token: Option<String> = row
            .get(4)
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get read_token: {e}")))?;

        // Enforce the unguessable token match. Legacy blobs without a
        // token are open (backward compatibility).
        if let Some(expected) = read_token.as_deref()
            && expected != token
        {
            return Err(StorageError::NotFound(format!("blob {blob_id}")));
        }

        Ok(BlobMeta {
            blob_id: id,
            mime_type,
            size: size as u64,
            created_at,
            read_token,
        })
    }

    async fn delete(&self, blob_id: &str) -> Result<()> {
        let _ = split_id(blob_id)?;
        let conn = self.pool.conn();
        conn.execute(
            "UPDATE blobs SET deleted_at = ?2 \
             WHERE blob_id = ?1 AND deleted_at IS NULL",
            libsql::params![blob_id.to_string(), now_unix()],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql blob delete: {e}")))?;
        Ok(())
    }
}

async fn path_exists(p: &Path) -> bool {
    tokio::fs::metadata(p).await.is_ok()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Per-attempt unique scratch filename for the streaming put path.
/// `pid` + monotonic counter + nanosecond timestamp keep two concurrent
/// uploads of identical content from sharing a temp path during the
/// hash-then-rename window.
fn unique_tmp_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let rand = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    format!("inflight.{pid}.{n}.{rand:x}.tmp")
}

/// 16-byte (128-bit) random hex token mixed into the blob ID. With
/// per-blob entropy this large, an attacker holding only a content
/// digest can't probe IDs across tenant boundaries — the token is the
/// read capability. `rand::random()` pulls from the OS-seeded thread
/// RNG, which is what we want for unguessability.
fn unique_read_token() -> String {
    let bytes: [u8; 16] = rand::random();
    hex_encode(&bytes)
}

/// Split a `blob_id` into `(hex_digest, token)`. Legacy IDs without
/// a token return an empty string for the second part.
fn split_id(blob_id: &str) -> Result<(&str, &str)> {
    let hex_all = blob_id
        .strip_prefix(SHA256_PREFIX)
        .ok_or_else(|| StorageError::NotFound(format!("invalid blob_id {blob_id}")))?;
    let (hex, token) = hex_all.split_once('.').unwrap_or((hex_all, ""));
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StorageError::NotFound(format!("invalid blob_id {blob_id}")));
    }
    Ok((hex, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn build() -> (LibsqlBlobStore, tempfile::TempDir) {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = LibsqlBlobStore::open(pool, dir.path().join("blobs"))
            .await
            .unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn put_get_stat_roundtrip() {
        let (store, _dir) = build().await;
        let bytes = b"hello world".to_vec();
        let blob_ref = store.put(&bytes, "text/plain", None).await.unwrap();
        assert!(blob_ref.blob_id.starts_with(SHA256_PREFIX));

        let got = store.get(&blob_ref.blob_id).await.unwrap();
        assert_eq!(got, bytes);

        let meta = store.stat(&blob_ref.blob_id).await.unwrap();
        assert_eq!(meta.blob_id, blob_ref.blob_id);
        assert_eq!(meta.mime_type, "text/plain");
        assert_eq!(meta.size, bytes.len() as u64);
    }

    #[tokio::test]
    async fn put_of_same_content_yields_distinct_capability_ids() {
        // Each put mints a fresh `read_token` so the resulting
        // `blob_id` (which embeds the token) is distinct even when the
        // bytes are identical. The on-disk file is content-addressed
        // and shared, but the metadata rows are per-call so two
        // tenants uploading the same image get two unforgeable ids.
        let (store, _dir) = build().await;
        let a = store.put(b"same", "image/png", None).await.unwrap();
        let b = store.put(b"same", "image/jpeg", None).await.unwrap();
        assert_ne!(
            a.blob_id, b.blob_id,
            "distinct read tokens produce distinct ids",
        );
        assert_eq!(store.get(&a.blob_id).await.unwrap(), b"same");
        assert_eq!(store.get(&b.blob_id).await.unwrap(), b"same");
        // Each metadata row carries its own caller-supplied mime.
        assert_eq!(store.stat(&a.blob_id).await.unwrap().mime_type, "image/png");
        assert_eq!(
            store.stat(&b.blob_id).await.unwrap().mime_type,
            "image/jpeg"
        );
    }

    #[tokio::test]
    async fn get_missing_returns_not_found() {
        let (store, _dir) = build().await;
        let id = format!("{SHA256_PREFIX}{}", "0".repeat(64));
        let err = store.get(&id).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn invalid_id_shape_is_not_found() {
        let (store, _dir) = build().await;
        for bad in [
            "sha256:short",
            "md5:0000000000000000000000000000000000000000000000000000000000000000",
            "no-prefix-hex",
        ] {
            let err = store.get(bad).await.unwrap_err();
            assert!(matches!(err, StorageError::NotFound(_)), "{bad}");
        }
    }

    #[tokio::test]
    async fn delete_then_get_is_not_found() {
        let (store, _dir) = build().await;
        let blob = store.put(b"bye", "text/plain", None).await.unwrap();
        store.delete(&blob.blob_id).await.unwrap();
        let err = store.get(&blob.blob_id).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_does_not_affect_a_fresh_put_of_same_content() {
        // Soft-delete revival via shared `blob_id` is gone with the
        // capability-id model — each put is its own row keyed by a
        // fresh token. This test pins the new contract: deleting one
        // capability for some content must not block a different
        // tenant from uploading the same content and getting their
        // own (independent, readable) id.
        let (store, _dir) = build().await;
        let first = store.put(b"shared", "text/plain", None).await.unwrap();
        store.delete(&first.blob_id).await.unwrap();
        let second = store.put(b"shared", "text/markdown", None).await.unwrap();
        assert_ne!(first.blob_id, second.blob_id);
        // The deleted capability stays dead.
        assert!(matches!(
            store.get(&first.blob_id).await,
            Err(StorageError::NotFound(_))
        ));
        // The new capability reads back the same on-disk bytes.
        assert_eq!(store.get(&second.blob_id).await.unwrap(), b"shared");
        assert_eq!(
            store.stat(&second.blob_id).await.unwrap().mime_type,
            "text/markdown",
        );
    }

    #[tokio::test]
    async fn delete_missing_is_idempotent() {
        let (store, _dir) = build().await;
        let id = format!("{SHA256_PREFIX}{}", "0".repeat(64));
        store.delete(&id).await.unwrap();
        // Calling again is fine.
        store.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn put_locks_down_dir_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        // Loosen the test process's umask so default-creating a file
        // would otherwise land at 0o644 — proves we set the mode
        // explicitly rather than relying on umask.
        unsafe { libc::umask(0o022) };

        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let blob_root = dir.path().join("blobs");
        let store = LibsqlBlobStore::open(pool, &blob_root).await.unwrap();

        let blob = store.put(b"perm check", "text/plain", None).await.unwrap();

        // Root must be 0o700.
        let mode = std::fs::metadata(&blob_root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "blob root mode {mode:o}");

        // Shard dir 0o700. The blob id is `sha256:<hex>.<token>`; the
        // on-disk path is content-addressed by `<hex>` only — the
        // token is metadata, not part of the filename.
        let hex_with_token = blob.blob_id.strip_prefix(SHA256_PREFIX).unwrap();
        let hex = hex_with_token
            .split_once('.')
            .map(|(h, _)| h)
            .unwrap_or(hex_with_token);
        let shard = blob_root.join(&hex[..2]);
        let mode = std::fs::metadata(&shard).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "shard mode {mode:o}");

        // Final file 0o600.
        let file = shard.join(hex);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "blob file mode {mode:o}");

        // Scratch dir (created by put_stream) 0o700 too.
        let scratch = blob_root.join(".tmp");
        if scratch.exists() {
            let mode = std::fs::metadata(&scratch).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "scratch mode {mode:o}");
        }
    }

    #[tokio::test]
    async fn put_stream_writes_chunks_and_round_trips_content() {
        // Streaming and buffered paths land on the same on-disk file
        // (content-addressed shard), so each one's `get` must return
        // the same bytes — even though the capability ids differ.
        let (store, _dir) = build().await;
        let chunks = [
            Bytes::from_static(b"hello "),
            Bytes::from_static(b"streaming "),
            Bytes::from_static(b"world"),
        ];
        let stream = stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)).boxed();
        let blob = store
            .put_stream(stream, "text/plain", None, u64::MAX)
            .await
            .unwrap();

        let direct = store
            .put(b"hello streaming world", "text/plain", None)
            .await
            .unwrap();
        assert_ne!(
            blob.blob_id, direct.blob_id,
            "fresh capabilities are minted per call",
        );
        assert_eq!(
            store.get(&blob.blob_id).await.unwrap(),
            b"hello streaming world",
        );
        assert_eq!(
            store.get(&direct.blob_id).await.unwrap(),
            b"hello streaming world",
        );
    }

    #[tokio::test]
    async fn put_stream_enforces_byte_cap_incrementally() {
        let (store, _dir) = build().await;
        let chunks = vec![Bytes::from(vec![b'a'; 10]), Bytes::from(vec![b'b'; 20])];
        let stream = stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)).boxed();
        let result = store
            .put_stream(stream, "application/octet-stream", None, 16)
            .await;
        match result {
            Err(StorageError::TooLarge { limit, actual }) => {
                assert_eq!(limit, 16);
                assert!(actual > limit);
            }
            Err(other) => panic!("expected TooLarge, got {other:?}"),
            Ok(_) => panic!("expected TooLarge, got Ok"),
        }
    }

    #[tokio::test]
    async fn put_stream_cleans_up_tmp_on_oversize() {
        let (store, dir) = build().await;
        let chunks = vec![Bytes::from(vec![0u8; 32])];
        let result = store
            .put_stream(
                stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>)).boxed(),
                "application/octet-stream",
                None,
                8,
            )
            .await;
        assert!(matches!(result, Err(StorageError::TooLarge { .. })));
        // Give the spawned cleanup task a turn.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Scratch dir should be empty: no orphan inflight.* files.
        let scratch = dir.path().join("blobs").join(".tmp");
        if scratch.exists() {
            let mut left = tokio::fs::read_dir(&scratch).await.unwrap();
            assert!(left.next_entry().await.unwrap().is_none(), "tmp leaked");
        }
    }

    #[tokio::test]
    async fn open_streams_blob_bytes() {
        use tokio::io::AsyncReadExt;
        let (store, _dir) = build().await;
        let blob = store
            .put(b"streaming download", "text/plain", None)
            .await
            .unwrap();
        let mut reader = store.open(&blob.blob_id).await.expect("open");
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"streaming download");
    }

    #[tokio::test]
    async fn open_returns_not_found_for_missing() {
        let (store, _dir) = build().await;
        let id = format!("{SHA256_PREFIX}{}", "0".repeat(64));
        match store.open(&id).await {
            Err(StorageError::NotFound(_)) => {}
            Err(other) => panic!("expected NotFound, got {other:?}"),
            Ok(_) => panic!("expected NotFound, got Ok"),
        }
    }

    #[tokio::test]
    async fn concurrent_puts_of_same_content_each_get_unique_capabilities() {
        // Regression for the codex adversarial-review finding: two
        // writers used to share `<hex>.tmp` and one rename could fire
        // ENOENT when the other consumed the temp first. With the
        // capability-id model, each writer gets its own unforgeable
        // id and no rename ever conflicts.
        let (store, _dir) = build().await;
        let store = std::sync::Arc::new(store);
        let bytes = b"identical content";
        let mut handles = Vec::new();
        for _ in 0..16 {
            let s = std::sync::Arc::clone(&store);
            handles.push(tokio::spawn(async move {
                s.put(bytes, "text/plain", None).await
            }));
        }
        let mut ids = Vec::new();
        for h in handles {
            ids.push(h.await.unwrap().expect("concurrent put").blob_id);
        }
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "every put gets a fresh capability");
        // All ids resolve to the same on-disk file content.
        for id in &ids {
            assert_eq!(store.get(id).await.unwrap(), bytes);
        }
    }
}
