//! Bounded retry wrapper for cross-process sqlite contention.
//!
//! Second line of defence, behind the connection's `busy_timeout` (see
//! `sqlite::BUSY_TIMEOUT`).
//!
//! *Intra-process* write contention never reaches sqlite: writers queue
//! on `PoolInner::writer` inside `SqlitePool::interact_write`, so a
//! second pooled writer waits its turn in the process instead of
//! colliding on the file lock and burning the busy handler's sleeps.
//!
//! What neither the queue nor the timeout covers is a *cross-process*
//! writer holding the lock for longer than the timeout — the CLI's
//! `baybo channels bot add/remove`, say, writing the same database file
//! a gateway is live on. A `BUSY` that survives both is therefore, by
//! construction, that case: genuinely anomalous, and worth a bounded
//! retry plus a loud log. That is why `interact_write` wraps *every*
//! write in this helper rather than a hand-picked few — there is no
//! longer a class of in-process contention for it to paper over.
//!
//! Every retry logs at `warn!` so operators and telemetry can see
//! contention when it matters; after the last attempt the original
//! error propagates for loud failure.
//!
//! Reads are not wrapped. Under WAL a reader takes no write lock, so it
//! has nothing to lose the race for, and retrying one would hide a real
//! fault rather than a contended moment.

use std::fmt::Display;
use std::future::Future;
use std::time::Duration;

/// Total backoff budget ≈ 30 + 60 + 120 = ~210ms before the final
/// attempt's error propagates. Short enough that operators don't
/// wait noticeably on a clean run, long enough to absorb realistic
/// gateway-vs-CLI collisions (single-statement writes <1ms each).
const MAX_ATTEMPTS: u32 = 4;
const INITIAL_BACKOFF_MS: u64 = 15;

/// Run `f` up to [`MAX_ATTEMPTS`] times, re-invoking it with
/// exponential backoff only when the error reads as a sqlite BUSY
/// condition (heuristic string match). Any other error
/// short-circuits.
///
/// Generic over the error type so call-sites can share the helper
/// across storage-level errors (`baybo_storage::StorageError`) and the
/// vault's security wrapper (`baybo_security::SecurityError`) without
/// either crate taking a dependency on the other.
///
/// `op` names the call-site ("vault.store_secret",
/// "channel_bots.put"). Appears in every retry's warn log.
pub async fn retry_on_busy<F, Fut, T, E>(op: &'static str, mut f: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: Display,
{
    let mut attempt: u32 = 0;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt + 1 < MAX_ATTEMPTS && is_busy_string(&e.to_string()) => {
                attempt += 1;
                let wait_ms = INITIAL_BACKOFF_MS << attempt; // 30, 60, 120
                tracing::warn!(
                    op,
                    attempt,
                    wait_ms,
                    error = %e,
                    "sqlite contention; retrying",
                );
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Heuristic — sqlite surfaces BUSY with a message containing
/// `"database is locked"` or `"database is busy"`. There's no typed
/// error code to match against short of depending on `rusqlite::Error`
/// directly (which would leak the storage backend into every
/// consumer), so we match on the string. Stable across driver
/// versions because the phrasing comes straight from upstream sqlite.
fn is_busy_string(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("database is locked") || lower.contains("database is busy")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use baybo_store::StorageError;

    fn busy_err() -> StorageError {
        StorageError::Internal(anyhow::anyhow!("sqlite upsert: database is locked"))
    }

    fn non_retryable_err() -> StorageError {
        StorageError::NotFound("nothing".into())
    }

    #[tokio::test]
    async fn succeeds_on_first_try_without_sleeping() {
        let calls = Mutex::new(0u32);
        let got = retry_on_busy("test.succeed", || async {
            *calls.lock().unwrap() += 1;
            Ok::<_, StorageError>(42)
        })
        .await
        .unwrap();
        assert_eq!(got, 42);
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_busy_up_to_max_attempts_then_propagates() {
        let calls = Mutex::new(0u32);
        let result: Result<(), _> = retry_on_busy("test.always_busy", || async {
            *calls.lock().unwrap() += 1;
            Err::<(), _>(busy_err())
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), MAX_ATTEMPTS);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_busy_then_succeeds() {
        let calls = Mutex::new(0u32);
        let got = retry_on_busy("test.flaky", || async {
            let mut n = calls.lock().unwrap();
            *n += 1;
            if *n < 3 {
                Err::<u32, _>(busy_err())
            } else {
                Ok(7)
            }
        })
        .await
        .unwrap();
        assert_eq!(got, 7);
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn non_busy_error_fails_fast_without_retry() {
        let calls = Mutex::new(0u32);
        let result: Result<(), _> = retry_on_busy("test.other", || async {
            *calls.lock().unwrap() += 1;
            Err::<(), _>(non_retryable_err())
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn works_with_non_storage_error_types() {
        // Verify the generic bound — SecurityError's BUSY string
        // (from SecretVault's wrapper) is matched the same way.
        #[derive(Debug)]
        struct SecErr(String);
        impl std::fmt::Display for SecErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        let calls = Mutex::new(0u32);
        let result: Result<(), _> = retry_on_busy("test.security", || async {
            *calls.lock().unwrap() += 1;
            Err::<(), _>(SecErr("Storage(sqlite insert: database is locked)".into()))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), MAX_ATTEMPTS);
    }

    #[test]
    fn is_busy_string_matches_common_sqlite_phrasings() {
        assert!(is_busy_string("sqlite upsert: database is locked"));
        assert!(is_busy_string("database is busy"));
        assert!(is_busy_string("DATABASE IS LOCKED"));
        assert!(!is_busy_string("not found: nothing"));
        assert!(!is_busy_string("disk full"));
    }
}
