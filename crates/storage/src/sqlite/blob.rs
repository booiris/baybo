use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use async_trait::async_trait;
use baybo_model::BlobRef;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::blob::{BlobMeta, BlobReader, BlobStore, ByteStream, Result, SHA256_PREFIX};

// `DirBuilder::mode(0o700)` and `OpenOptions::mode(0o600)` lock down
// every directory and file in the blob tree to the owning UID — the
// channel-token boundary is the only gate on user-uploaded media, and
// world-readable bytes on disk would bypass it. Baybo is Unix-only (see
// `CLAUDE.md`) so neither needs a cfg gate.
const BLOB_PATH_LOCK_STRIPES: usize = 64;

const INFLIGHT_TMP_DIR: &str = ".tmp";

/// A scratch entry older than this at construction is an orphan from a
/// crashed mid-upload process and gets reaped; anything younger may
/// belong to a live upload in another process and is left alone.
const INFLIGHT_TMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

pub struct SqliteBlobStore {
    pool: SqlitePool,
    root: PathBuf,
    path_locks: Arc<Vec<tokio::sync::Mutex<()>>>,
}

impl SqliteBlobStore {
    /// Construct a blob store backed by `pool` for metadata and `root`
    /// on the filesystem for byte payloads. The caller is responsible
    /// for creating `root` (or using [`Self::open`] which does it for
    /// you).
    pub fn new(pool: SqlitePool, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        reap_stale_inflight_tmp(&root);
        Self {
            pool,
            root,
            path_locks: new_path_locks(),
        }
    }

    /// Construct a blob store and ensure `root` exists on disk.
    pub async fn open(pool: SqlitePool, root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .map_err(|e| anyhow::anyhow!("failed to create blob root {}: {e}", root.display()))?;
        Ok(Self::new(pool, root))
    }

    fn blob_path(&self, hex: &str, ext: &str) -> PathBuf {
        // 2-char shard prefix to keep one directory from accumulating
        // every blob — same shape git uses for its objects. The
        // extension is purely cosmetic: the canonical id is the hash,
        // but `image.jpg` is more useful than a 64-hex string when
        // someone is poking around the state directory by hand.
        let mut name = String::with_capacity(hex.len() + ext.len());
        name.push_str(hex);
        name.push_str(ext);
        self.root.join(&hex[..2]).join(name)
    }

    /// Top-level scratch directory for in-flight streaming uploads.
    /// Lives under the blob root with a leading dot so the 2-char hex
    /// shard scan can never collide. Contents are unique-per-attempt
    /// and reaped on success (rename) or error (explicit cleanup).
    fn scratch_dir(&self) -> PathBuf {
        self.root.join(INFLIGHT_TMP_DIR)
    }

    async fn content_path_guard(&self, hex: &str, ext: &str) -> tokio::sync::MutexGuard<'_, ()> {
        let mut hasher = DefaultHasher::new();
        hex.hash(&mut hasher);
        ext.hash(&mut hasher);
        let idx = (hasher.finish() as usize) % self.path_locks.len();
        self.path_locks[idx].lock().await
    }

    /// Persist the metadata row + return the `BlobRef`. Idempotent:
    /// `ON CONFLICT … DO UPDATE` keeps the latest mime + bumps the LRU
    /// timestamp.
    async fn record_metadata(
        &self,
        hex: &str,
        mime_type: &str,
        size: u64,
        uploader_identity: Option<&str>,
        read_token: &str,
    ) -> Result<BlobRef> {
        let blob_id = format!("{SHA256_PREFIX}{hex}.{read_token}");
        let now = now_unix();
        let row_id = blob_id.clone();
        let mime_type = mime_type.to_string();
        let uploader_identity = uploader_identity.map(|s| s.to_string());
        let read_token = read_token.to_string();
        self.pool
            .interact("blobs.record_metadata", move |conn| {
                conn.execute(
                    "INSERT INTO blobs (blob_id, mime_type, size, uploader_identity, read_token, created_at, last_accessed_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6) \
                     ON CONFLICT(blob_id) DO UPDATE SET \
                        mime_type = excluded.mime_type, \
                        last_accessed_at = excluded.last_accessed_at",
                    rusqlite::params![
                        row_id,
                        mime_type,
                        size as i64,
                        uploader_identity,
                        read_token,
                        now,
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(BlobRef { blob_id })
    }

    /// Bump the LRU timestamp on read. Best-effort: a touch failure
    /// must never bubble up to the caller — the read path needs to keep
    /// returning bytes even when the DB is briefly unwritable.
    async fn touch(&self, blob_id: &str) {
        let id = blob_id.to_string();
        let now = now_unix();
        if let Err(e) = self
            .pool
            .interact("blobs.touch", move |conn| {
                conn.execute(
                    "UPDATE blobs SET last_accessed_at = ?2 \
                     WHERE blob_id = ?1",
                    rusqlite::params![id, now],
                )?;
                Ok(())
            })
            .await
        {
            tracing::warn!(
                blob_id = %redacted_blob_id(blob_id),
                error = %e,
                "blob touch failed",
            );
        }
    }
}

/// Construction-time reap of upload scratch leaked by a crash mid-upload
/// (success renames the file away, error paths delete it — only a crash
/// strands one). Best-effort: entries past `INFLIGHT_TMP_MAX_AGE` by
/// mtime are removed, younger ones stay untouched, and any I/O failure
/// is ignored — the store must construct regardless.
fn reap_stale_inflight_tmp(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root.join(INFLIGHT_TMP_DIR)) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let stale = now
            .duration_since(mtime)
            .map(|age| age > INFLIGHT_TMP_MAX_AGE)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn new_path_locks() -> Arc<Vec<tokio::sync::Mutex<()>>> {
    Arc::new(
        (0..BLOB_PATH_LOCK_STRIPES)
            .map(|_| tokio::sync::Mutex::new(()))
            .collect(),
    )
}

fn now_unix() -> i64 {
    super::time::now_us()
}

/// Cosmetic file extension to append to a blob's on-disk filename so a
/// human poking around `<state>/blobs/` can recognise an image vs a
/// PDF without consulting the sqlite metadata. Returns `""` when the
/// MIME type isn't in the table — anything we don't recognise lands
/// without a suffix rather than guessing wrong.
fn mime_extension(mime: &str) -> &'static str {
    // Strip parameters (`image/jpeg; charset=utf-8`) and lowercase
    // before lookup so casing / params don't change the on-disk path.
    let bare = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match bare.as_str() {
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/heic" => ".heic",
        "image/heif" => ".heif",
        "image/svg+xml" => ".svg",
        "image/bmp" => ".bmp",
        "audio/ogg" | "audio/opus" => ".ogg",
        "audio/mpeg" => ".mp3",
        "audio/mp4" | "audio/m4a" => ".m4a",
        "audio/wav" | "audio/x-wav" => ".wav",
        "audio/flac" => ".flac",
        "audio/silk" => ".silk",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "video/quicktime" => ".mov",
        "video/x-matroska" => ".mkv",
        "application/pdf" => ".pdf",
        "application/zip" => ".zip",
        "application/json" => ".json",
        "application/x-tar" => ".tar",
        "application/gzip" | "application/x-gzip" => ".gz",
        "text/plain" => ".txt",
        "text/html" => ".html",
        "text/csv" => ".csv",
        "text/markdown" => ".md",
        _ => "",
    }
}

#[async_trait]
impl BlobStore for SqliteBlobStore {
    async fn list_ids_by_uploader(
        &self,
        prefix: &str,
        older_than_us: Option<i64>,
    ) -> Result<Vec<String>> {
        let lower = prefix.to_string();
        // Successor bound: an identity extending `prefix` sorts below `prefix`
        // with a trailing 0x7F (DEL) — hex/uuid chars are all < 0x7F — so this
        // stays an indexed range scan on `idx_blobs_uploader_identity_size`,
        // never a full-table filter.
        let upper = format!("{prefix}\u{7f}");
        self.pool
            .interact("blobs.list_by_uploader", move |conn| {
                let mut ids = Vec::new();
                match older_than_us {
                    Some(cut) => {
                        let mut stmt = conn.prepare(
                            "SELECT blob_id FROM blobs \
                             WHERE uploader_identity >= ?1 AND uploader_identity < ?2 \
                               AND created_at < ?3",
                        )?;
                        let rows = stmt.query_map(rusqlite::params![lower, upper, cut], |r| {
                            r.get::<_, String>(0)
                        })?;
                        for r in rows {
                            ids.push(r?);
                        }
                    }
                    None => {
                        let mut stmt = conn.prepare(
                            "SELECT blob_id FROM blobs \
                             WHERE uploader_identity >= ?1 AND uploader_identity < ?2",
                        )?;
                        let rows = stmt.query_map(rusqlite::params![lower, upper], |r| {
                            r.get::<_, String>(0)
                        })?;
                        for r in rows {
                            ids.push(r?);
                        }
                    }
                }
                Ok(ids)
            })
            .await
    }

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
        let ext = mime_extension(mime_type);
        let final_path = self.blob_path(&hex, ext);
        let _path_guard = self.content_path_guard(&hex, ext).await;
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
        // stat first so we have the recorded mime to reconstruct the
        // on-disk extension the put path appended (and so a missing
        // metadata row surfaces as NotFound before we try the disk).
        let meta = self.stat(blob_id).await?;
        let path = self.blob_path(hex, mime_extension(&meta.mime_type));
        tokio::fs::read(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(format!(
                    "blob bytes missing for {}",
                    redacted_blob_id(blob_id),
                ))
            } else {
                StorageError::Internal(anyhow::anyhow!("read blob {}: {e}", path.display()))
            }
        })
    }

    async fn open(&self, blob_id: &str) -> Result<BlobReader> {
        self.open_at(blob_id, 0).await
    }

    async fn open_at(&self, blob_id: &str, offset: u64) -> Result<BlobReader> {
        let (hex, _token) = split_id(blob_id)?;
        // Re-enter stat() so the read_token gate is enforced on the offset/resume
        // path too — the on-disk file is keyed by hex alone, so a seek-by-hex
        // shortcut would let a caller holding only the bare digest resume a read.
        let meta = self.stat(blob_id).await?;
        let path = self.blob_path(hex, mime_extension(&meta.mime_type));
        let mut file = tokio::fs::File::open(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(format!(
                    "blob bytes missing for {}",
                    redacted_blob_id(blob_id),
                ))
            } else {
                StorageError::Internal(anyhow::anyhow!("open blob {}: {e}", path.display()))
            }
        })?;
        if offset > 0 {
            use tokio::io::AsyncSeekExt;
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("seek blob {}: {e}", path.display()))
                })?;
        }
        Ok(Box::pin(file))
    }

    async fn stat(&self, blob_id: &str) -> Result<BlobMeta> {
        let (_hex, token) = split_id(blob_id)?;
        let id_param = blob_id.to_string();
        let row = self
            .pool
            .interact("blobs.stat", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT blob_id, mime_type, size, created_at, read_token \
                         FROM blobs WHERE blob_id = ?1",
                        rusqlite::params![id_param],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, Option<String>>(4)?,
                            ))
                        },
                    )
                    .optional()?)
            })
            .await?;

        let (id, mime_type, size, created_at, read_token) = row
            .ok_or_else(|| StorageError::NotFound(format!("blob {}", redacted_blob_id(blob_id))))?;

        // Enforce the unguessable token match. Legacy blobs without a
        // token are open (backward compatibility).
        if let Some(expected) = read_token.as_deref()
            && expected != token
        {
            return Err(StorageError::NotFound(format!(
                "blob {}",
                redacted_blob_id(blob_id),
            )));
        }

        // LRU touch: refresh the access timestamp. No sweep consumes it
        // yet — blobs are never GC'd today — but keeping it accurate
        // means a future janitor LRU sweep can trust the column from day
        // one. Done only after the token check passes so a probe with a
        // wrong token wouldn't extend a victim's TTL under that sweep.
        self.touch(blob_id).await;

        Ok(BlobMeta {
            blob_id: id,
            mime_type,
            size: size as u64,
            created_at,
            read_token,
        })
    }

    async fn delete(&self, blob_id: &str) -> Result<()> {
        let (hex, _token) = split_id(blob_id)?;

        // Capture mime so we can locate the on-disk file before the
        // metadata row is gone, then check whether any other live row
        // still points at the same content path.
        let id_param = blob_id.to_string();
        let mime_type: Option<String> = self
            .pool
            .interact("blobs.delete_lookup", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT mime_type FROM blobs WHERE blob_id = ?1",
                        rusqlite::params![id_param],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .await?;

        if let Some(mime) = mime_type {
            let ext = mime_extension(&mime);
            let path = self.blob_path(hex, ext);
            let _path_guard = self.content_path_guard(hex, ext).await;
            let id_param = blob_id.to_string();
            let affected = self
                .pool
                .interact("blobs.delete", move |conn| {
                    Ok(conn.execute(
                        "DELETE FROM blobs WHERE blob_id = ?1",
                        rusqlite::params![id_param],
                    )?)
                })
                .await?;
            // Two puts of the same content (and same mime) share one
            // on-disk file by content addressing; deleting one
            // capability must not yank the file out from under the
            // other. Confirm the path is unreferenced before unlinking.
            if affected > 0 && !self.any_live_for_path(hex, ext).await.unwrap_or(false) {
                match tokio::fs::remove_file(&path).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "blob payload unlink failed",
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

impl SqliteBlobStore {
    /// Return `true` if some other live blob row resolves to the same
    /// on-disk path (i.e. shares this `(hex, mime_extension)` pair).
    /// Used by [`BlobStore::delete`] before unlinking a payload so a
    /// concurrent capability for the same content doesn't lose its
    /// bytes.
    async fn any_live_for_path(&self, hex: &str, ext: &'static str) -> Result<bool> {
        // PK range on the digest instead of a full-table scan: `blob_id` is the
        // primary key `sha256:<hex>.<token>`, so `[sha256:<hex>, sha256:<hex>~)`
        // is served by the PK index and touches ONLY same-digest rows. `~`
        // (0x7e) sorts above `.` and every hex digit; a 64-hex digest can't
        // prefix another, so no cross-digest bleed. No trailing `.` on the lower
        // bound keeps a legacy tokenless `sha256:<hex>` id in range. This keeps
        // the per-delete cost O(rows-for-this-digest), so the janitor's deck
        // sweep can't go quadratic against the whole (chat + deck) blob table.
        let lower = format!("{SHA256_PREFIX}{hex}");
        let upper = format!("{SHA256_PREFIX}{hex}~");
        self.pool
            .interact("blobs.any_live_for_path", move |conn| {
                let mut stmt = conn
                    .prepare("SELECT mime_type FROM blobs WHERE blob_id >= ?1 AND blob_id < ?2")?;
                let mut rows = stmt.query(rusqlite::params![lower, upper])?;
                while let Some(row) = rows.next()? {
                    let other_mime: String = row.get(0)?;
                    if mime_extension(&other_mime) == ext {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .await
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

fn redacted_blob_id(blob_id: &str) -> String {
    let Some(rest) = blob_id.strip_prefix(SHA256_PREFIX) else {
        return "<invalid>".to_string();
    };
    let hex = rest.split_once('.').map(|(hex, _)| hex).unwrap_or(rest);
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        format!("{SHA256_PREFIX}{hex}.<redacted>")
    } else {
        "<invalid>".to_string()
    }
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
        .ok_or_else(|| StorageError::NotFound("invalid blob_id".to_string()))?;
    let (hex, token) = hex_all.split_once('.').unwrap_or((hex_all, ""));
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StorageError::NotFound(format!(
            "invalid blob_id {}",
            redacted_blob_id(blob_id),
        )));
    }
    Ok((hex, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn build() -> (SqliteBlobStore, tempfile::TempDir) {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteBlobStore::open(pool, dir.path().join("blobs"))
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
    async fn id_without_token_or_with_wrong_token_is_not_found() {
        // Holding the SHA-256 alone (or a wrong token) is not enough to read the
        // blob. Every byte-surfacing path must reject a doctored id that strips
        // or mutates the token suffix.
        let (store, _dir) = build().await;
        let real = store.put(b"private", "text/plain", None).await.unwrap();

        // Sanity: legitimate id round-trips.
        assert_eq!(store.get(&real.blob_id).await.unwrap(), b"private");

        let hex_with_token = real.blob_id.strip_prefix(SHA256_PREFIX).unwrap();
        let (hex, token) = hex_with_token.split_once('.').unwrap();
        assert_eq!(token.len(), 32, "token should be 32 hex chars (16 bytes)");

        // Variants an attacker could plausibly construct.
        let bad_ids = [
            // Hex only — no token.
            format!("{SHA256_PREFIX}{hex}"),
            // Hex + empty token.
            format!("{SHA256_PREFIX}{hex}."),
            // Hex + a different (well-formed-looking) token. Same
            // shape as a real id, different bits — must not unlock
            // the file.
            format!("{SHA256_PREFIX}{hex}.{}", "f".repeat(32)),
            // Hex + a one-bit-flipped token (off-by-one).
            format!(
                "{SHA256_PREFIX}{hex}.{}{}",
                if token.starts_with('0') { "1" } else { "0" },
                &token[1..],
            ),
        ];
        for bad in &bad_ids {
            match store.get(bad).await {
                Err(StorageError::NotFound(_)) => {}
                other => panic!("get({bad}) leaked: {other:?}"),
            }
            match store.stat(bad).await {
                Err(StorageError::NotFound(_)) => {}
                other => panic!("stat({bad}) leaked: {other:?}"),
            }
            match store.open(bad).await {
                Err(StorageError::NotFound(_)) => {}
                Err(other) => panic!("open({bad}) leaked: {other:?}"),
                Ok(_) => panic!("open({bad}) leaked Ok"),
            }
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
        // Each put is its own row keyed by a fresh token under the
        // capability-id model. Deleting one capability for some
        // content must not block a different tenant from uploading
        // the same content and getting their own (independent,
        // readable) id.
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
    async fn list_ids_by_uploader_ranges_on_identity_and_age() {
        let (store, _dir) = build().await;
        let a = store
            .put(b"a", "text/plain", Some("deck:card1"))
            .await
            .unwrap();
        let _bu = store
            .put(b"b", "text/plain", Some("deck-user:card1"))
            .await
            .unwrap();
        let _dev = store
            .put(b"c", "text/plain", Some("device:x"))
            .await
            .unwrap();

        // "deck:" catches only the service-produced blob — never `deck-user:`
        // (picker uploads) nor `device:` (chat).
        assert_eq!(
            store.list_ids_by_uploader("deck:", None).await.unwrap(),
            vec![a.blob_id.clone()]
        );
        // An exact card identity targets that one card.
        assert_eq!(
            store
                .list_ids_by_uploader("deck:card1", None)
                .await
                .unwrap(),
            vec![a.blob_id.clone()]
        );
        // Age cutoff: nothing predates a far-past cutoff; everything predates a
        // far-future one.
        let now = chrono::Utc::now().timestamp_micros();
        assert!(
            store
                .list_ids_by_uploader("deck:", Some(now - 10_000_000_000))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .list_ids_by_uploader("deck:", Some(now + 10_000_000_000))
                .await
                .unwrap(),
            vec![a.blob_id]
        );
    }

    #[tokio::test]
    async fn put_locks_down_dir_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        // Loosen the test process's umask so default-creating a file
        // would otherwise land at 0o644 — proves we set the mode
        // explicitly rather than relying on umask.
        unsafe { libc::umask(0o022) };

        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let blob_root = dir.path().join("blobs");
        let store = SqliteBlobStore::open(pool, &blob_root).await.unwrap();

        let blob = store.put(b"perm check", "text/plain", None).await.unwrap();

        // Root must be 0o700.
        let mode = std::fs::metadata(&blob_root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "blob root mode {mode:o}");

        // Shard dir 0o700. The blob id is `sha256:<hex>.<token>`; the
        // on-disk path is content-addressed by `<hex>` plus a cosmetic
        // mime-derived extension (`.txt` here) — the token never
        // appears on disk.
        let hex_with_token = blob.blob_id.strip_prefix(SHA256_PREFIX).unwrap();
        let hex = hex_with_token
            .split_once('.')
            .map(|(h, _)| h)
            .unwrap_or(hex_with_token);
        let shard = blob_root.join(&hex[..2]);
        let mode = std::fs::metadata(&shard).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "shard mode {mode:o}");

        // Final file 0o600 — name is `<hex>.txt` for `text/plain`.
        let file = shard.join(format!("{hex}.txt"));
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
    async fn on_disk_filename_carries_mime_derived_extension() {
        // The `blob_id` stays content-addressed, but the on-disk
        // filename gets a cosmetic suffix so a human can identify the
        // file at a glance. Round-trips must still work because the
        // read path reconstructs the same suffix from the recorded
        // mime in the metadata row.
        let (store, dir) = build().await;
        let blob = store
            .put(b"\xFF\xD8\xFF\xE0", "image/jpeg", None)
            .await
            .unwrap();
        let hex_with_token = blob.blob_id.strip_prefix(SHA256_PREFIX).unwrap();
        let hex = hex_with_token.split_once('.').map(|(h, _)| h).unwrap();
        let path = dir
            .path()
            .join("blobs")
            .join(&hex[..2])
            .join(format!("{hex}.jpg"));
        assert!(path.exists(), "expected {} to exist", path.display());
        // Read back through the public api — proves the get path
        // reconstructs the same filename from the stored mime.
        let got = store.get(&blob.blob_id).await.unwrap();
        assert_eq!(got, b"\xFF\xD8\xFF\xE0");
    }

    #[tokio::test]
    async fn unknown_mime_lands_without_extension() {
        // Unrecognized mimes (e.g. `application/x-baybo-foo`) must not
        // synthesis a fake suffix — the disk filename falls back to
        // the bare hash. Both put and get round-trip.
        let (store, dir) = build().await;
        let blob = store
            .put(b"opaque", "application/x-baybo-foo", None)
            .await
            .unwrap();
        let hex_with_token = blob.blob_id.strip_prefix(SHA256_PREFIX).unwrap();
        let hex = hex_with_token.split_once('.').map(|(h, _)| h).unwrap();
        let path = dir.path().join("blobs").join(&hex[..2]).join(hex);
        assert!(path.exists(), "expected {} to exist", path.display());
        let got = store.get(&blob.blob_id).await.unwrap();
        assert_eq!(got, b"opaque");
    }

    #[test]
    fn mime_extension_table_is_case_and_param_tolerant() {
        // Lookup must ignore casing and `;`-suffixed parameters.
        assert_eq!(mime_extension("image/jpeg"), ".jpg");
        assert_eq!(mime_extension("IMAGE/JPEG"), ".jpg");
        assert_eq!(mime_extension("image/jpeg; charset=binary"), ".jpg");
        assert_eq!(mime_extension("audio/ogg"), ".ogg");
        assert_eq!(mime_extension("application/pdf"), ".pdf");
        assert_eq!(mime_extension("video/mp4"), ".mp4");
        assert_eq!(mime_extension("application/x-not-known"), "");
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
    async fn construction_reaps_only_stale_inflight_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        let tmp = root.join(INFLIGHT_TMP_DIR);
        std::fs::create_dir_all(&tmp).unwrap();
        let stale = tmp.join("inflight.1.0.aaaa.tmp");
        let fresh = tmp.join("inflight.2.0.bbbb.tmp");
        std::fs::write(&stale, b"crashed upload").unwrap();
        std::fs::write(&fresh, b"live upload").unwrap();
        let old_mtime = std::time::SystemTime::now()
            - (INFLIGHT_TMP_MAX_AGE + std::time::Duration::from_secs(3600));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_accessed(old_mtime)
                .set_modified(old_mtime),
        )
        .unwrap();
        drop(file);

        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let _store = SqliteBlobStore::open(pool, &root).await.unwrap();

        assert!(!stale.exists(), "stale inflight tmp reaped at construction");
        assert!(
            fresh.exists(),
            "young tmp may belong to a live upload elsewhere — untouched"
        );
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
