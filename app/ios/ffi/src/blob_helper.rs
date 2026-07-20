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

async fn cache_uploaded_bytes(expected_hex: &str, bytes: &[u8]) -> Result<(), String> {
    if !is_sha256_hex(expected_hex) {
        return Err("invalid blob cache digest".into());
    }
    let entry = cache_entry_for_hex(expected_hex.to_owned()).await?;
    let cache_lock = cache_lock(&entry);
    let guard = cache_lock.lock().await;
    let result: Result<(), String> = async {
        if cache_exists(&entry).await {
            return Ok(());
        }

        let upload_part_path = upload_part_path(&entry);
        if let Err(e) = tokio::fs::write(&upload_part_path, bytes).await {
            let _ = tokio::fs::remove_file(&upload_part_path).await;
            return Err(format!("write cached blob: {e}"));
        }
        if let Err(e) = tokio::fs::rename(&upload_part_path, entry.path()).await {
            let _ = tokio::fs::remove_file(&upload_part_path).await;
            return Err(format!("rename cached blob: {e}"));
        }
        Ok(())
    }
    .await;
    drop(guard);
    forget_cache_lock_if_idle(&entry, &cache_lock);
    result
}

pub(crate) async fn cache_uploaded_bytes_best_effort(expected_hex: &str, bytes: &[u8]) {
    if let Err(e) = cache_uploaded_bytes(expected_hex, bytes).await {
        log::warn!("cache uploaded blob: {e}");
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
    let mut buf = vec![0u8; 64 * 1024];
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
