//! Shared local-file helpers for blob cache semantics.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::OnceCell;

use crate::core::blob_id_sha256_hex;

const BLOB_CACHE_SUBDIR: &str = "baybo-blob-cache";
const UPLOAD_PART_SUFFIX: &str = "upload.part";

/// Hard cap on one blob, the same 100 MiB the gateway enforces
/// (`baybo_store::blob::MAX_BLOB_BYTES`) and Swift mirrors
/// (`ChatStore.maxAttachmentBytes`). Restated rather than imported: pulling
/// `baybo-store` in would link sqlite into the phone for one integer.
pub(crate) const MAX_BLOB_BYTES: u64 = 100 * 1024 * 1024;

/// How much of a file one read pulls — the digest pass and the direct leg's
/// body chunks. The relay leg reads in `MAX_TUNNEL_CHUNK` instead, which is
/// its frame ceiling, not a read size.
pub(crate) const FILE_READ_CHUNK_BYTES: usize = 64 * 1024;

static UPLOAD_PART_COUNTER: AtomicU64 = AtomicU64::new(0);
static UPLOAD_PART_SWEEP: OnceCell<()> = OnceCell::const_new();
static CACHE_LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
static BLOB_CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Where downloaded blobs live. Set once at client construction from
/// `ClientConfig.blob_cache_dir`; host tests (and any embedder that leaves it
/// unset) fall back to the OS temp dir.
///
/// Nothing evicts from here. The blobs are durable on purpose — a file the user
/// downloaded stays downloaded — so the directory only ever grows. When that
/// becomes a problem it wants a real retention policy, not a surprise sweep.
pub(crate) fn set_blob_cache_dir(dir: PathBuf) {
    let _ = BLOB_CACHE_DIR.set(dir);
}

fn blob_cache_dir() -> PathBuf {
    BLOB_CACHE_DIR.get().cloned().unwrap_or_else(fallback_dir)
}

#[cfg(not(test))]
fn fallback_dir() -> PathBuf {
    std::env::temp_dir().join(BLOB_CACHE_SUBDIR)
}

/// Under test the fallback is per-PROCESS, and that is load-bearing.
/// `sweep_stale_upload_parts_once` deletes every `*.upload.part` it finds: it
/// exists to clear parts a previous crashed RUN left behind, and it cannot tell
/// a dead run's leftovers from another live process's in-flight write. The app
/// is one process, so that holds. `cargo nextest` is not — it runs each test in
/// its own process, and on a shared fallback dir one test's sweep deletes
/// another's live part mid-write, the rename finds no source, and the read comes
/// back `NotFound`. Test-only: production behaviour is unchanged.
#[cfg(test)]
fn fallback_dir() -> PathBuf {
    std::env::temp_dir().join(format!("{BLOB_CACHE_SUBDIR}-{}", std::process::id()))
}

#[derive(Clone)]
pub(crate) struct BlobCacheEntry {
    expected_hex: String,
    path: PathBuf,
    part_path: PathBuf,
}

impl BlobCacheEntry {
    pub(crate) fn expected_hex(&self) -> &str {
        &self.expected_hex
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn part_path(&self) -> &Path {
        &self.part_path
    }
}

pub(crate) async fn cache_entry(blob_id: &str) -> Result<BlobCacheEntry, String> {
    let expected_hex = blob_id_sha256_hex(blob_id)
        .filter(|hex| is_sha256_hex(hex))
        .ok_or_else(|| "invalid blob id".to_string())?
        .to_owned();
    cache_entry_for_hex(expected_hex).await
}

pub(crate) fn bytes_sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The cache entry a just-uploaded blob should land in, or `None` when it is
/// already cached and there is nothing to write.
async fn upload_cache_entry(expected_hex: &str) -> Result<Option<BlobCacheEntry>, String> {
    if !is_sha256_hex(expected_hex) {
        return Err("invalid blob cache digest".into());
    }
    let entry = cache_entry_for_hex(expected_hex.to_owned()).await?;
    if cache_exists(&entry).await {
        return Ok(None);
    }
    Ok(Some(entry))
}

/// Publish a filled part file under its digest name, so the file the user just
/// sent answers "ready" instead of offering a download arrow. The rename is the
/// only step a reader can observe, so no reader ever sees a partial blob; the
/// per-digest lock keeps it from racing a concurrent download's own rename.
/// A part that does not get renamed is removed either way.
async fn publish_upload_part(entry: &BlobCacheEntry, part_path: &Path) -> Result<(), String> {
    let cache_lock = cache_lock(entry);
    let guard = cache_lock.lock().await;
    let renamed = if cache_exists(entry).await {
        Ok(false)
    } else {
        tokio::fs::rename(part_path, entry.path())
            .await
            .map(|()| true)
            .map_err(|e| format!("rename cached blob: {e}"))
    };
    drop(guard);
    forget_cache_lock_if_idle(entry, &cache_lock);
    if !matches!(renamed, Ok(true)) {
        let _ = tokio::fs::remove_file(part_path).await;
    }
    renamed.map(|_| ())
}

async fn cache_uploaded_bytes(expected_hex: &str, bytes: &[u8]) -> Result<(), String> {
    let Some(entry) = upload_cache_entry(expected_hex).await? else {
        return Ok(());
    };
    let part_path = upload_part_path(&entry);
    if let Err(e) = tokio::fs::write(&part_path, bytes).await {
        let _ = tokio::fs::remove_file(&part_path).await;
        return Err(format!("write cached blob: {e}"));
    }
    publish_upload_part(&entry, &part_path).await
}

pub(crate) async fn cache_uploaded_bytes_best_effort(expected_hex: &str, bytes: &[u8]) {
    if let Err(e) = cache_uploaded_bytes(expected_hex, bytes).await {
        log::warn!("cache uploaded blob: {e}");
    }
}

/// Mirrors a streaming upload's body bytes into the cache as they go, and
/// publishes them once the upload is confirmed.
///
/// A tee off the body pass, never a copy of the source afterwards: a copy is a
/// THIRD independent read, so a file that is still being written (an in-flight
/// screen recording, an iCloud item syncing a new version) would land under a
/// digest that is not its own. Nothing evicts from this directory and no reader
/// re-hashes, so one such entry serves wrong bytes for that digest forever. The
/// teed bytes are hashed as they are written and the part file is published only
/// if that digest is the name it would take.
///
/// Best effort throughout: a cache failure costs the user a re-download, never
/// the upload they just made.
#[derive(Clone)]
pub(crate) struct UploadCacheTee(Arc<tokio::sync::Mutex<Option<TeePart>>>);

struct TeePart {
    entry: BlobCacheEntry,
    part_path: PathBuf,
    file: Option<tokio::fs::File>,
    hasher: Sha256,
}

impl Drop for TeePart {
    fn drop(&mut self) {
        // A part outlives its tee only when the upload failed or its task was
        // cancelled, and nothing sweeps this directory until the next process
        // start. Blocking `unlink` because a `Drop` cannot await.
        let _ = std::fs::remove_file(&self.part_path);
    }
}

impl UploadCacheTee {
    pub(crate) async fn open(expected_hex: &str) -> Self {
        let part = match open_tee_part(expected_hex).await {
            Ok(part) => part,
            Err(e) => {
                log::warn!("cache uploaded blob: {e}");
                None
            }
        };
        Self(Arc::new(tokio::sync::Mutex::new(part)))
    }

    /// An inert tee, for readers that are not feeding an upload.
    #[cfg(test)]
    fn off() -> Self {
        Self(Arc::new(tokio::sync::Mutex::new(None)))
    }

    /// Mirror one body chunk. A write failure retires the tee: the upload runs
    /// on, the blob simply does not end up cached.
    async fn write(&self, chunk: &[u8]) {
        let mut slot = self.0.lock().await;
        let Some(part) = slot.as_mut() else {
            return;
        };
        let Some(file) = part.file.as_mut() else {
            return;
        };
        if let Err(e) = file.write_all(chunk).await {
            log::warn!("cache uploaded blob: write part: {e}");
            slot.take();
            return;
        }
        part.hasher.update(chunk);
    }

    /// The upload is confirmed: publish what streamed through, under the digest
    /// of those very bytes.
    pub(crate) async fn publish(&self) {
        let Some(part) = self.0.lock().await.take() else {
            return;
        };
        if let Err(e) = publish_tee_part(part).await {
            log::warn!("cache uploaded blob: {e}");
        }
    }
}

async fn open_tee_part(expected_hex: &str) -> Result<Option<TeePart>, String> {
    let Some(entry) = upload_cache_entry(expected_hex).await? else {
        return Ok(None);
    };
    let part_path = upload_part_path(&entry);
    let file = tokio::fs::File::create(&part_path)
        .await
        .map_err(|e| format!("create part: {e}"))?;
    Ok(Some(TeePart {
        entry,
        part_path,
        file: Some(file),
        hasher: Sha256::new(),
    }))
}

async fn publish_tee_part(mut part: TeePart) -> Result<(), String> {
    if let Some(mut file) = part.file.take() {
        file.flush().await.map_err(|e| format!("flush part: {e}"))?;
    }
    let actual_hex = hex::encode(std::mem::take(&mut part.hasher).finalize());
    if actual_hex != part.entry.expected_hex() {
        return Err("uploaded blob cache digest mismatch".into());
    }
    publish_upload_part(&part.entry, &part.part_path).await
}

/// A file's content digest and byte length.
#[derive(Debug)]
pub(crate) struct FileDigest {
    pub(crate) hex: String,
    pub(crate) len: u64,
}

/// Hash `path` and measure it, rejecting an over-cap file before any network
/// work. Both upload legs read the file TWICE — once here, once for the body —
/// because the relay head must declare `Content-SHA256` before the first body
/// byte leaves.
pub(crate) async fn hash_upload_file(path: &Path) -> Result<FileDigest, String> {
    hash_upload_file_capped(path, MAX_BLOB_BYTES).await
}

async fn hash_upload_file_capped(path: &Path, cap: u64) -> Result<FileDigest, String> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("read attachment: {e}"))?;
    if !meta.is_file() {
        return Err("attachment is not a file".into());
    }
    if meta.len() > cap {
        return Err(over_cap_message(cap));
    }
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open attachment: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; FILE_READ_CHUNK_BYTES];
    let mut len = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read attachment: {e}"))?;
        if n == 0 {
            break;
        }
        len += n as u64;
        // The stat above is a snapshot; a file still being written outgrows it.
        if len > cap {
            return Err(over_cap_message(cap));
        }
        hasher.update(&buf[..n]);
    }
    Ok(FileDigest {
        hex: hex::encode(hasher.finalize()),
        len,
    })
}

fn over_cap_message(cap: u64) -> String {
    format!("attachment is larger than the {cap} byte limit")
}

/// An upload's [`ProgressTicker`], shared between the body reader and the call
/// that owns the request.
///
/// Shared because the reader cannot report the end: reqwest stops polling the
/// body the instant the declared `Content-Length` is written, so the direct
/// leg's reader never sees its own EOF. Whoever knows the bytes are up calls
/// [`Self::finish`].
#[derive(Clone)]
pub(crate) struct UploadTicker(Arc<Mutex<ProgressTicker>>);

impl UploadTicker {
    pub(crate) fn new(progress: ProgressSink, total: u64) -> Self {
        Self(Arc::new(Mutex::new(ProgressTicker::new(
            progress,
            Some(total),
            0,
        ))))
    }

    fn advance(&self, n: usize) {
        self.0.lock().advance(n);
    }

    pub(crate) fn finish(&self) {
        self.0.lock().finish();
    }
}

/// Sequential body chunks for a streaming upload, ticking the same rate-limited
/// [`ProgressTicker`] the download path uses and teeing every chunk into the
/// blob cache.
///
/// Reads exactly the length [`hash_upload_file`] measured: that is what both
/// legs declared as their content length, and a file that shrank underneath us
/// would otherwise leave the body one frame short forever.
pub(crate) struct UploadReader {
    file: tokio::fs::File,
    remaining: u64,
    chunk_bytes: usize,
    ticker: UploadTicker,
    tee: UploadCacheTee,
}

impl UploadReader {
    pub(crate) async fn open(
        path: &Path,
        total: u64,
        chunk_bytes: usize,
        ticker: UploadTicker,
        tee: UploadCacheTee,
    ) -> Result<Self, String> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| format!("open attachment: {e}"))?;
        Ok(Self {
            file,
            remaining: total,
            chunk_bytes,
            ticker,
            tee,
        })
    }

    /// The next body chunk, `None` once the declared length has been read.
    pub(crate) async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let want = self.remaining.min(self.chunk_bytes as u64) as usize;
        let mut buf = vec![0u8; want];
        let n = self
            .file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read attachment: {e}"))?;
        if n == 0 {
            return Err("attachment changed while uploading".into());
        }
        buf.truncate(n);
        self.remaining -= n as u64;
        self.tee.write(&buf).await;
        self.ticker.advance(n);
        Ok(Some(buf))
    }
}

pub(crate) async fn read_or_download_blob_bytes<D, Fut>(
    blob_id: String,
    download_to_path: D,
) -> Result<Vec<u8>, String>
where
    D: FnOnce(String, BlobCacheEntry) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let entry = cache_entry(&blob_id).await?;
    if !cache_exists(&entry).await {
        let cache_lock = cache_lock(&entry);
        let guard = cache_lock.lock().await;
        let result: Result<(), String> = async {
            if !cache_exists(&entry).await {
                download_to_path(blob_id.clone(), entry.clone()).await?;
            }
            Ok(())
        }
        .await;
        drop(guard);
        forget_cache_lock_if_idle(&entry, &cache_lock);
        result?;
    }
    read_cached(&entry).await
}

pub(crate) fn ensure_blob_id_matches(expected_hex: &str, blob_id: &str) -> Result<(), String> {
    let actual_hex = blob_id_sha256_hex(blob_id)
        .filter(|hex| is_sha256_hex(hex))
        .ok_or_else(|| "invalid blob id returned".to_string())?;
    if actual_hex == expected_hex {
        Ok(())
    } else {
        Err("uploaded blob id digest mismatch".into())
    }
}

async fn cache_entry_for_hex(expected_hex: String) -> Result<BlobCacheEntry, String> {
    let dir = blob_cache_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create cache dir: {e}"))?;
    sweep_stale_upload_parts_once(dir.clone()).await;
    let path = dir.join(&expected_hex);
    let part_path = path.with_extension("part");
    Ok(BlobCacheEntry {
        expected_hex,
        path,
        part_path,
    })
}

fn upload_part_path(entry: &BlobCacheEntry) -> PathBuf {
    let counter = UPLOAD_PART_COUNTER.fetch_add(1, Ordering::Relaxed);
    entry.path().with_extension(format!(
        "{}.{}.{}",
        std::process::id(),
        counter,
        UPLOAD_PART_SUFFIX
    ))
}

fn cache_lock(entry: &BlobCacheEntry) -> Arc<tokio::sync::Mutex<()>> {
    let locks = CACHE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock();
    locks
        .entry(entry.expected_hex().to_owned())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn forget_cache_lock_if_idle(entry: &BlobCacheEntry, cache_lock: &Arc<tokio::sync::Mutex<()>>) {
    if Arc::strong_count(cache_lock) > 2 {
        return;
    }
    let Some(locks) = CACHE_LOCKS.get() else {
        return;
    };
    let mut locks = locks.lock();
    if let Some(current) = locks.get(entry.expected_hex())
        && Arc::ptr_eq(current, cache_lock)
        && Arc::strong_count(current) == 2
    {
        locks.remove(entry.expected_hex());
    }
}

async fn sweep_stale_upload_parts_once(dir: PathBuf) {
    UPLOAD_PART_SWEEP
        .get_or_init(|| async move {
            let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
                return;
            };
            loop {
                match entries.next_entry().await {
                    Ok(Some(entry)) => {
                        let name = entry.file_name();
                        if name.to_string_lossy().ends_with(UPLOAD_PART_SUFFIX) {
                            let _ = tokio::fs::remove_file(entry.path()).await;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        })
        .await;
}

async fn cache_exists(entry: &BlobCacheEntry) -> bool {
    tokio::fs::try_exists(entry.path()).await.unwrap_or(false)
}

/// Is this blob already on disk? Never downloads. The cache lives under the
/// OS temp dir, which iOS purges under storage pressure, so a caller must ask
/// again rather than remembering a `true` from earlier.
pub(crate) async fn is_cached(blob_id: &str) -> bool {
    match cache_entry(blob_id).await {
        Ok(entry) => cache_exists(&entry).await,
        Err(_) => false,
    }
}

/// `sha256:<64 lower-hex digest>.<≥1 lower-hex read token>` — the blob
/// capability id shape the store mints. Anything else can never resolve, so the
/// deck display serve rejects it before any cache/network work.
pub(crate) fn is_valid_blob_id_shape(blob_id: &str) -> bool {
    let Some(rest) = blob_id.strip_prefix("sha256:") else {
        return false;
    };
    let Some((digest, token)) = rest.split_once('.') else {
        return false;
    };
    let is_lower_hex = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    digest.len() == 64 && is_lower_hex(digest) && is_lower_hex(token)
}

/// Read a cached blob's bytes with NO network and NO active-binding check —
/// the deck display path's fast path, so a card image renders even while the
/// device is unbound/offline. `None` when the id is malformed or not cached.
pub(crate) async fn read_cached_bytes(blob_id: &str) -> Option<Vec<u8>> {
    let entry = cache_entry(blob_id).await.ok()?;
    if !cache_exists(&entry).await {
        return None;
    }
    read_cached(&entry).await.ok()
}

/// The size of a cached blob without reading it (a `stat`) — lets the deck
/// display route reject an over-cap blob before materializing it. `None` when
/// the id is malformed or not cached.
pub(crate) async fn cached_size(blob_id: &str) -> Option<u64> {
    let entry = cache_entry(blob_id).await.ok()?;
    tokio::fs::metadata(entry.path())
        .await
        .ok()
        .map(|m| m.len())
}

/// A foreign progress observer, absent when the caller doesn't want ticks.
pub(crate) type ProgressSink = Option<Arc<dyn crate::api::BlobProgress>>;

/// Chunks arrive every few KiB. Coalesce them: each tick crosses the FFI and
/// (on iOS) a webview bridge, and no one can read faster than this anyway.
const PROGRESS_TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Rate-limits `BlobProgress::on_progress` over a chunked download. Ticks on
/// construction (so a resumed download opens at its floor, not zero), at most
/// once per [`PROGRESS_TICK_INTERVAL`] while bytes flow, and once on `finish`.
pub(crate) struct ProgressTicker {
    sink: ProgressSink,
    total: Option<u64>,
    downloaded: u64,
    interval: Duration,
    last_emit: Instant,
}

impl ProgressTicker {
    pub(crate) fn new(sink: ProgressSink, total: Option<u64>, already_on_disk: u64) -> Self {
        Self::with_interval(sink, total, already_on_disk, PROGRESS_TICK_INTERVAL)
    }

    fn with_interval(
        sink: ProgressSink,
        total: Option<u64>,
        already_on_disk: u64,
        interval: Duration,
    ) -> Self {
        let ticker = Self {
            sink,
            total,
            downloaded: already_on_disk,
            interval,
            last_emit: Instant::now(),
        };
        ticker.emit();
        ticker
    }

    fn emit(&self) {
        if let Some(sink) = &self.sink {
            sink.on_progress(self.downloaded, self.total);
        }
    }

    /// Account for `n` freshly written bytes, emitting only when the interval
    /// has elapsed. Returns whether it emitted (the tests read this).
    pub(crate) fn advance(&mut self, n: usize) -> bool {
        self.downloaded = self.downloaded.saturating_add(n as u64);
        if self.sink.is_none() || self.last_emit.elapsed() < self.interval {
            return false;
        }
        self.last_emit = Instant::now();
        self.emit();
        true
    }

    /// The download completed: report the final byte count even if the last
    /// chunk landed inside the throttle window.
    pub(crate) fn finish(&self) {
        self.emit();
    }
}

async fn read_cached(entry: &BlobCacheEntry) -> Result<Vec<u8>, String> {
    tokio::fs::read(entry.path())
        .await
        .map_err(|e| format!("read cached blob: {e}"))
}

pub(crate) async fn hash_existing_part(entry: &BlobCacheEntry) -> Result<(Sha256, u64), String> {
    let mut hasher = Sha256::new();
    let mut file = match tokio::fs::File::open(entry.part_path()).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((hasher, 0)),
        Err(e) => return Err(format!("open part for resume: {e}")),
    };
    let mut total = 0u64;
    let mut buf = vec![0u8; FILE_READ_CHUNK_BYTES];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("read part for resume: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hasher, total))
}

pub(crate) async fn open_part_append(entry: &BlobCacheEntry) -> Result<tokio::fs::File, String> {
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(entry.part_path())
        .await
        .map_err(|e| format!("open part: {e}"))
}

pub(crate) async fn remove_part(entry: &BlobCacheEntry) {
    let _ = tokio::fs::remove_file(entry.part_path()).await;
}

pub(crate) async fn finalize_download(
    mut file: tokio::fs::File,
    entry: &BlobCacheEntry,
    hasher: Sha256,
) -> Result<(), String> {
    let actual_hex = hex::encode(hasher.finalize());
    if actual_hex != entry.expected_hex() {
        drop(file);
        remove_part(entry).await;
        return Err("content digest mismatch".into());
    }
    file.flush().await.map_err(|e| format!("flush part: {e}"))?;
    drop(file);
    tokio::fs::rename(entry.part_path(), entry.path())
        .await
        .map_err(|e| format!("rename part -> dest: {e}"))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingProgress {
        ticks: Mutex<Vec<(u64, Option<u64>)>>,
    }

    impl crate::api::BlobProgress for RecordingProgress {
        fn on_progress(&self, downloaded: u64, total: Option<u64>) {
            self.ticks.lock().push((downloaded, total));
        }
    }

    fn ticker_with(
        interval: Duration,
        total: Option<u64>,
        already: u64,
    ) -> (ProgressTicker, Arc<RecordingProgress>) {
        let sink = Arc::new(RecordingProgress::default());
        let ticker = ProgressTicker::with_interval(
            Some(sink.clone() as Arc<dyn crate::api::BlobProgress>),
            total,
            already,
            interval,
        );
        (ticker, sink)
    }

    /// A resumed download must open at what is already on disk. Reporting 0
    /// first would snap the reader's byte counter backwards.
    #[test]
    fn a_resumed_download_opens_at_its_floor() {
        let (_ticker, sink) = ticker_with(Duration::ZERO, Some(900), 400);
        assert_eq!(sink.ticks.lock().as_slice(), [(400, Some(900))]);
    }

    #[test]
    fn blob_id_shape_accepts_capability_ids_and_rejects_the_rest() {
        let digest = "a".repeat(64);
        assert!(is_valid_blob_id_shape(&format!("sha256:{digest}.deadbeef")));
        assert!(is_valid_blob_id_shape(&format!("sha256:{digest}.0")));
        for bad in [
            "",
            "sha256:",
            "md5:abcdef",
            "abcdef",
            "sha256:.tok",                                  // empty digest
            &format!("sha256:{digest}."),                   // empty token
            &format!("sha256:{}.deadbeef", "a".repeat(63)), // short digest
            &format!("sha256:{}.deadbeef", "A".repeat(64)), // upper-hex digest
            &format!("sha256:{digest}.XYZ"),                // non-hex token
        ] {
            assert!(!is_valid_blob_id_shape(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn ticks_are_throttled_and_the_last_byte_always_lands() {
        let (mut ticker, sink) = ticker_with(Duration::from_secs(3600), Some(30), 0);
        for _ in 0..3 {
            assert!(!ticker.advance(10), "the window has not elapsed");
        }
        // Only the opening tick so far; `finish` is what reports the tail.
        assert_eq!(sink.ticks.lock().as_slice(), [(0, Some(30))]);
        ticker.finish();
        assert_eq!(
            sink.ticks.lock().as_slice(),
            [(0, Some(30)), (30, Some(30))]
        );
    }

    #[test]
    fn a_zero_interval_ticks_every_chunk() {
        let (mut ticker, sink) = ticker_with(Duration::ZERO, None, 0);
        assert!(ticker.advance(5));
        assert!(ticker.advance(7));
        assert_eq!(
            sink.ticks.lock().as_slice(),
            [(0, None), (5, None), (12, None)]
        );
    }

    /// The whole point of the config knob: blobs must land where the embedder
    /// said, not in the OS temp dir iOS purges. Sets the process-wide `OnceLock`,
    /// so it is the only test allowed to.
    #[tokio::test]
    async fn the_configured_cache_dir_is_where_blobs_land() {
        let nonce = UPLOAD_PART_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("baybo-configured-cache-{nonce}"));
        set_blob_cache_dir(dir.clone());

        let hex = "a".repeat(64);
        let entry = cache_entry(&format!("sha256:{hex}.tok"))
            .await
            .expect("cache entry");

        assert_eq!(entry.path().parent(), Some(dir.as_path()));
        assert!(
            !entry
                .path()
                .starts_with(std::env::temp_dir().join(BLOB_CACHE_SUBDIR)),
            "the temp fallback must not win once a dir is configured"
        );
    }

    /// The common case: an image thumbnail nobody is watching. The ticker must
    /// not pay for a tick it has nowhere to send.
    #[test]
    fn no_sink_means_no_work() {
        let mut ticker = ProgressTicker::with_interval(None, Some(10), 0, Duration::ZERO);
        assert!(!ticker.advance(10));
        ticker.finish();
    }

    /// A scratch file nobody else can collide with: the cache dir is global and
    /// nextest runs each test in its own process.
    async fn scratch_file(tag: &str, bytes: &[u8]) -> PathBuf {
        let nonce = UPLOAD_PART_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("baybo-upload-{tag}-{}-{nonce}", std::process::id()));
        tokio::fs::write(&path, bytes).await.expect("seed file");
        path
    }

    #[tokio::test]
    async fn a_files_digest_and_length_match_its_bytes() {
        let bytes = vec![7u8; FILE_READ_CHUNK_BYTES * 2 + 13];
        let path = scratch_file("digest", &bytes).await;

        let digest = hash_upload_file(&path).await.expect("hash");

        assert_eq!(digest.hex, bytes_sha256_hex(&bytes));
        assert_eq!(digest.len, bytes.len() as u64);
        let _ = tokio::fs::remove_file(&path).await;
    }

    /// The cap is enforced before any network work, so an over-cap pick costs
    /// the user nothing but the stat.
    #[tokio::test]
    async fn an_over_cap_file_is_refused_by_its_size() {
        let path = scratch_file("overcap", &[1u8; 64]).await;

        let err = hash_upload_file_capped(&path, 32)
            .await
            .expect_err("over cap");

        assert_eq!(err, "attachment is larger than the 32 byte limit");
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn a_file_exactly_at_the_cap_still_uploads() {
        let bytes = vec![3u8; 32];
        let path = scratch_file("atcap", &bytes).await;

        let digest = hash_upload_file_capped(&path, 32).await.expect("hash");

        assert_eq!(digest.len, 32);
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn a_missing_path_fails_before_anything_is_opened() {
        let path = std::env::temp_dir().join("baybo-upload-does-not-exist");
        let _ = tokio::fs::remove_file(&path).await;

        let err = hash_upload_file(&path).await.expect_err("missing");

        assert!(err.starts_with("read attachment: "), "{err}");
    }

    #[tokio::test]
    async fn a_directory_is_not_an_attachment() {
        let err = hash_upload_file(&std::env::temp_dir())
            .await
            .expect_err("directory");
        assert_eq!(err, "attachment is not a file");
    }

    /// The upload's ticks ride the SAME 100ms limiter as a download: three
    /// chunks read, but only the opening and closing ticks cross the FFI.
    #[tokio::test]
    async fn an_upload_reads_the_declared_length_and_rate_limits_its_ticks() {
        let bytes: Vec<u8> = (0..25u8).collect();
        let path = scratch_file("reader", &bytes).await;
        let sink = Arc::new(RecordingProgress::default());

        let ticker = UploadTicker::new(
            Some(sink.clone() as Arc<dyn crate::api::BlobProgress>),
            bytes.len() as u64,
        );
        let mut reader = UploadReader::open(
            &path,
            bytes.len() as u64,
            10,
            ticker.clone(),
            UploadCacheTee::off(),
        )
        .await
        .expect("open");

        let mut chunks = Vec::new();
        while let Some(chunk) = reader.next_chunk().await.expect("chunk") {
            chunks.push(chunk);
        }
        ticker.finish();

        assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), [10, 10, 5]);
        assert_eq!(chunks.concat(), bytes);
        assert_eq!(
            sink.ticks.lock().as_slice(),
            [(0, Some(25)), (25, Some(25))]
        );
        let _ = tokio::fs::remove_file(&path).await;
    }

    /// A body one frame short would hang the relay leg waiting for `last`, and
    /// make the direct leg's declared Content-Length a lie.
    #[tokio::test]
    async fn a_file_that_shrank_under_the_upload_is_an_error() {
        let path = scratch_file("shrank", &[9u8; 8]).await;
        let mut reader = UploadReader::open(
            &path,
            64,
            8,
            UploadTicker::new(None, 64),
            UploadCacheTee::off(),
        )
        .await
        .expect("open");

        reader.next_chunk().await.expect("first chunk");
        let err = reader.next_chunk().await.expect_err("truncated");

        assert_eq!(err, "attachment changed while uploading");
        let _ = tokio::fs::remove_file(&path).await;
    }

    /// Content no other test in this process can produce, so the cache entry it
    /// hashes to is this test's alone.
    fn unique_bytes(tag: &str) -> Vec<u8> {
        let nonce = UPLOAD_PART_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("baybo-{tag}-{}-{nonce}", std::process::id()).into_bytes()
    }

    /// Everything in the cache dir named for `hex` — the published entry plus
    /// any part file left lying around.
    async fn cache_files_for(hex: &str) -> Vec<String> {
        let mut names = Vec::new();
        let Ok(mut dir) = tokio::fs::read_dir(blob_cache_dir()).await else {
            return names;
        };
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(hex) {
                names.push(name);
            }
        }
        names
    }

    /// Drain a whole file through a reader teeing into `tee`, as an upload's
    /// body pass does.
    async fn stream_body(path: &Path, len: u64, tee: UploadCacheTee) {
        let mut reader = UploadReader::open(path, len, 8, UploadTicker::new(None, len), tee)
            .await
            .expect("open");
        while reader.next_chunk().await.expect("chunk").is_some() {}
    }

    /// The upload cache's whole point: the file the user just sent must answer
    /// "ready" instead of showing a download arrow.
    #[tokio::test]
    async fn a_streamed_upload_lands_in_the_cache_under_its_digest() {
        let bytes = unique_bytes("upload-file-cache");
        let source = scratch_file("cachesrc", &bytes).await;
        let digest = hash_upload_file(&source).await.expect("hash");
        let entry = cache_entry_for_hex(digest.hex.clone())
            .await
            .expect("cache entry");

        let tee = UploadCacheTee::open(&digest.hex).await;
        stream_body(&source, digest.len, tee.clone()).await;
        tee.publish().await;

        assert_eq!(
            tokio::fs::read(entry.path()).await.expect("cached"),
            bytes,
            "what went on the wire is what lands"
        );
        assert_eq!(
            cache_files_for(&digest.hex).await,
            std::slice::from_ref(&digest.hex)
        );
        let _ = tokio::fs::remove_file(entry.path()).await;
        let _ = tokio::fs::remove_file(&source).await;
    }

    /// A file that changes under the upload — an in-progress screen recording,
    /// an iCloud item syncing a new version — must never publish an entry whose
    /// content is not what its name claims. Nothing evicts from this directory
    /// and no reader re-hashes, so one poisoned entry would serve wrong bytes
    /// for that digest to every consumer, forever.
    #[tokio::test]
    async fn a_source_that_changes_mid_upload_publishes_no_cache_entry() {
        let original = unique_bytes("upload-midflight");
        let source = scratch_file("midflight", &original).await;
        let digest = hash_upload_file(&source).await.expect("hash");
        let entry = cache_entry_for_hex(digest.hex.clone())
            .await
            .expect("cache entry");

        let tee = UploadCacheTee::open(&digest.hex).await;
        let rewritten = unique_bytes("upload-midflight-rewritten").repeat(4);
        assert!(
            rewritten.len() > original.len(),
            "grew, as a recording does"
        );
        tokio::fs::write(&source, &rewritten)
            .await
            .expect("rewrite");
        stream_body(&source, digest.len, tee.clone()).await;
        tee.publish().await;

        assert!(
            !cache_exists(&entry).await,
            "an entry whose bytes are not its digest must never be published"
        );
        assert_eq!(
            cache_files_for(&digest.hex).await,
            Vec::<String>::new(),
            "and its part file must not be left behind"
        );
        let _ = tokio::fs::remove_file(&source).await;
    }

    /// An upload that fails never calls `publish`, and its part file must not
    /// survive the attempt: this directory is swept only at process start.
    #[tokio::test]
    async fn an_upload_that_never_publishes_leaves_no_part_behind() {
        let bytes = unique_bytes("upload-abandoned");
        let source = scratch_file("abandoned", &bytes).await;
        let digest = hash_upload_file(&source).await.expect("hash");

        let tee = UploadCacheTee::open(&digest.hex).await;
        stream_body(&source, digest.len, tee.clone()).await;
        drop(tee);

        assert_eq!(cache_files_for(&digest.hex).await, Vec::<String>::new());
        let _ = tokio::fs::remove_file(&source).await;
    }

    #[tokio::test]
    async fn concurrent_upload_cache_writes_share_final_without_part_stomping() {
        let nonce = UPLOAD_PART_COUNTER.fetch_add(1, Ordering::Relaxed);
        let bytes = format!("baybo-upload-cache-race-{nonce}").into_bytes();
        let expected_hex = bytes_sha256_hex(&bytes);
        let entry = cache_entry_for_hex(expected_hex.clone())
            .await
            .expect("cache entry");
        let _ = tokio::fs::remove_file(entry.path()).await;
        let _ = tokio::fs::remove_file(entry.part_path()).await;

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let expected_hex = expected_hex.clone();
            let bytes = bytes.clone();
            tasks.push(tokio::spawn(async move {
                cache_uploaded_bytes(&expected_hex, &bytes).await
            }));
        }

        for task in tasks {
            task.await
                .expect("task joins")
                .expect("cache upload succeeds");
        }
        let cached = tokio::fs::read(entry.path()).await.expect("cached bytes");
        assert_eq!(cached, bytes);

        let _ = tokio::fs::remove_file(entry.path()).await;
        let _ = tokio::fs::remove_file(entry.part_path()).await;
    }

    #[tokio::test]
    async fn concurrent_download_cache_writes_share_final_without_part_stomping() {
        let nonce = UPLOAD_PART_COUNTER.fetch_add(1, Ordering::Relaxed);
        let bytes = format!("baybo-download-cache-race-{nonce}").into_bytes();
        let expected_hex = bytes_sha256_hex(&bytes);
        let blob_id = format!("sha256:{expected_hex}.test-token");
        let entry = cache_entry_for_hex(expected_hex)
            .await
            .expect("cache entry");
        let _ = tokio::fs::remove_file(entry.path()).await;
        let _ = tokio::fs::remove_file(entry.part_path()).await;

        let writes = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let blob_id = blob_id.clone();
            let bytes = bytes.clone();
            let writes = writes.clone();
            tasks.push(tokio::spawn(async move {
                read_or_download_blob_bytes(blob_id, move |_blob_id, entry| async move {
                    writes.fetch_add(1, Ordering::Relaxed);
                    let mut file = open_part_append(&entry).await?;
                    file.write_all(&bytes)
                        .await
                        .map_err(|e| format!("write part: {e}"))?;
                    let mut hasher = Sha256::new();
                    hasher.update(&bytes);
                    finalize_download(file, &entry, hasher).await
                })
                .await
            }));
        }

        for task in tasks {
            let cached = task
                .await
                .expect("task joins")
                .expect("download cache succeeds");
            assert_eq!(cached, bytes);
        }
        assert_eq!(writes.load(Ordering::Relaxed), 1);
        let cached = tokio::fs::read(entry.path()).await.expect("cached bytes");
        assert_eq!(cached, bytes);
        assert!(
            !tokio::fs::try_exists(entry.part_path())
                .await
                .unwrap_or(false)
        );

        let _ = tokio::fs::remove_file(entry.path()).await;
        let _ = tokio::fs::remove_file(entry.part_path()).await;
    }
}
