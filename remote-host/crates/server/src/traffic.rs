//! The durable traffic ledger.
//!
//! A single background task drains the relay's per-`(remote_api_key, server_id)`
//! and push's per-`device_id` in-memory counters on an interval and
//! **accumulates** them into a SQLite DB — a row's stored total is grown
//! by each interval's delta (`col = col + delta` via UPSERT), so the DB holds the
//! lifetime total and a process restart (in-memory counters reset to zero) simply
//! resumes adding. Snapshot-then-commit means a transient write failure retries the
//! same bytes next interval rather than losing or double-counting them.
//!
//! It is a **separate** DB file from the admission allow-list: that one is polled
//! read-only and edited out of band by an operator, so mixing 60s machine writes
//! into it would contend on SQLite's single writer and pollute a human-curated
//! table. With `TRAFFIC_DB_PATH` empty, persistence is skipped but the drain +
//! evict still runs each tick so the in-memory maps stay bounded.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use remote_host_edge::IpTrafficRegistry;
use remote_host_push::PushTrafficRegistry;
use remote_host_relay::TrafficRegistry;
use rusqlite::params;

use crate::sqlite::{SqliteError, SqlitePool};

// All three ledgers bucket by hour (`hour` = the UTC hour start, stamped by SQLite
// at write time as `YYYY-MM-DD HH:00:00`), so totals are an hourly time series the
// retention sweep can age out. The hour is part of every PRIMARY KEY (the UPSERT
// conflict target) and indexed for the prune. Old rows past the retention window
// are deleted (these are best-effort stats, not session data).

/// The three hourly ledger tables, named once so the writer (schema + UPSERTs +
/// prune) and the read layer (`traffic_query`) share a single source of truth.
pub(crate) const RELAY_TRAFFIC_TABLE: &str = "relay_traffic";
pub(crate) const PUSH_TRAFFIC_TABLE: &str = "push_traffic";
pub(crate) const IP_TRAFFIC_TABLE: &str = "ip_traffic";

/// How the current UTC hour start (`YYYY-MM-DD HH:00:00`) is stamped at write
/// time. Shared with `traffic_query` so an overview "last hour" lookup compares
/// against the byte-identical expression the UPSERTs stamp into `hour`.
pub(crate) const CURRENT_HOUR_EXPR: &str = "strftime('%Y-%m-%d %H:00:00','now')";

/// One row per `(remote_api_key, server_id, hour)`.
const RELAY_SCHEMA: &str = r#"CREATE TABLE IF NOT EXISTS relay_traffic (
    remote_api_key TEXT NOT NULL,
    server_id TEXT NOT NULL,
    hour TEXT NOT NULL,
    bytes_up INTEGER NOT NULL DEFAULT 0,
    bytes_down INTEGER NOT NULL DEFAULT 0,
    frames_up INTEGER NOT NULL DEFAULT 0,
    frames_down INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (remote_api_key, server_id, hour)
)"#;

/// One row per `(device_id, hour)`: APNs sends + payload bytes egressed that hour.
const PUSH_SCHEMA: &str = r#"CREATE TABLE IF NOT EXISTS push_traffic (
    device_id TEXT NOT NULL,
    hour TEXT NOT NULL,
    sends INTEGER NOT NULL DEFAULT 0,
    bytes INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (device_id, hour)
)"#;

/// One row per `(ip, endpoint, hour)`: request count + bytes from that source IP to
/// that endpoint that hour.
const IP_SCHEMA: &str = r#"CREATE TABLE IF NOT EXISTS ip_traffic (
    ip TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    hour TEXT NOT NULL,
    requests INTEGER NOT NULL DEFAULT 0,
    bytes INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (ip, endpoint, hour)
)"#;

/// `hour` indexes so the retention prune (`WHERE hour < …`) is a range scan, not a
/// full table scan.
const HOUR_INDEXES: [&str; 3] = [
    "CREATE INDEX IF NOT EXISTS idx_relay_traffic_hour ON relay_traffic(hour)",
    "CREATE INDEX IF NOT EXISTS idx_push_traffic_hour ON push_traffic(hour)",
    "CREATE INDEX IF NOT EXISTS idx_ip_traffic_hour ON ip_traffic(hour)",
];

/// Add an interval's relay delta onto the current hour's row (insert it on first
/// sight). `excluded.*` is the just-inserted delta. The current-hour stamp is
/// interpolated from [`CURRENT_HOUR_EXPR`] (a trusted const, never user input).
static RELAY_UPSERT: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"INSERT INTO relay_traffic
    (remote_api_key, server_id, hour, bytes_up, bytes_down, frames_up, frames_down, updated_at)
    VALUES (?1, ?2, {CURRENT_HOUR_EXPR}, ?3, ?4, ?5, ?6, datetime('now'))
ON CONFLICT(remote_api_key, server_id, hour) DO UPDATE SET
    bytes_up = bytes_up + excluded.bytes_up,
    bytes_down = bytes_down + excluded.bytes_down,
    frames_up = frames_up + excluded.frames_up,
    frames_down = frames_down + excluded.frames_down,
    updated_at = excluded.updated_at"#
    )
});

/// Add an interval's push delta onto the device's current-hour row.
static PUSH_UPSERT: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"INSERT INTO push_traffic (device_id, hour, sends, bytes, updated_at)
    VALUES (?1, {CURRENT_HOUR_EXPR}, ?2, ?3, datetime('now'))
ON CONFLICT(device_id, hour) DO UPDATE SET
    sends = sends + excluded.sends,
    bytes = bytes + excluded.bytes,
    updated_at = excluded.updated_at"#
    )
});

/// Add an interval's per-IP delta onto the `(ip, endpoint)`'s current-hour row.
static IP_UPSERT: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"INSERT INTO ip_traffic (ip, endpoint, hour, requests, bytes, updated_at)
    VALUES (?1, ?2, {CURRENT_HOUR_EXPR}, ?3, ?4, datetime('now'))
ON CONFLICT(ip, endpoint, hour) DO UPDATE SET
    requests = requests + excluded.requests,
    bytes = bytes + excluded.bytes,
    updated_at = excluded.updated_at"#
    )
});

/// Spawn the flush task. `db_path` empty ⇒ no persistence (drain-and-discard, still
/// evicts). `flush_secs` is clamped to ≥1. `retention_days` ages out hourly rows
/// older than that. The relay + ip registries are required (both always on); the
/// push one is `Some` only when the push role is mounted. `relay_max_tracked`
/// recomputes the relay entry cap (sized to the live admission connection capacity)
/// — applied before the first flush and re-applied each interval so the cap follows
/// hot-reloaded admission edits.
pub(crate) fn spawn<F>(
    db_path: String,
    flush_secs: u64,
    retention_days: u64,
    relay: Arc<TrafficRegistry>,
    push: Option<Arc<PushTrafficRegistry>>,
    ip: Arc<IpTrafficRegistry>,
    relay_max_tracked: F,
) where
    F: Fn() -> usize + Send + 'static,
{
    tokio::spawn(async move {
        let secs = flush_secs.max(1);
        let opened = if db_path.is_empty() {
            tracing::info!(
                "remote-host: traffic persistence disabled (empty TRAFFIC_DB_PATH); in-memory accounting + eviction only"
            );
            None
        } else {
            match init_db(&db_path).await {
                Ok(pool) => {
                    tracing::info!(path = %db_path, period_secs = secs, "remote-host: traffic ledger enabled");
                    Some(pool)
                }
                Err(e) => {
                    tracing::error!(path = %db_path, error = %e, "remote-host: traffic DB open failed; in-memory eviction only");
                    None
                }
            }
        };
        let pool = opened.as_ref();
        // Size the cap to the current admission capacity before the first leg can
        // connect; the loop keeps it fresh as admission hot-reloads.
        relay.set_max_tracked(relay_max_tracked());
        // Run the retention prune ~once an hour, not every flush — aging is in days,
        // so a per-minute sweep is wasted work. `ticks` starts at 0, so the first
        // flush also prunes.
        let prune_every = (3600 / secs).max(1);
        let mut ticks: u64 = 0;
        let mut tick = tokio::time::interval(Duration::from_secs(secs));
        tick.tick().await; // the first tick is immediate; nothing to flush yet
        loop {
            tick.tick().await;
            let cap = relay_max_tracked();
            relay.set_max_tracked(cap);
            flush_once(pool, &relay, push.as_deref(), &ip).await;
            // Surface saturation once per interval (never per meter_for): at the cap,
            // new (remote_api_key, server_id) pairs are recorded by an inert meter.
            if relay.tracked_len() >= cap {
                tracing::warn!(
                    cap,
                    "remote-host: relay traffic map at its capacity cap; new (remote_api_key, server_id) pairs are going unrecorded — likely relay_node_id churn or an undersized admission max_conns sum"
                );
            }
            if ip.tracked_len() >= ip.max_tracked() {
                tracing::warn!(
                    cap = ip.max_tracked(),
                    "remote-host: ip traffic map at its capacity cap; new (ip, endpoint) pairs are going unrecorded (IP churn flood?)"
                );
            }
            if let Some(p) = pool
                && ticks.is_multiple_of(prune_every)
                && let Err(e) = prune(p, retention_days).await
            {
                tracing::warn!(error = %e, "remote-host: traffic retention prune failed");
            }
            ticks = ticks.wrapping_add(1);
        }
    });
}

/// Open the local DB pool and ensure the schema ([`ensure_schema`]).
async fn init_db(path: &str) -> Result<SqlitePool, SqliteError> {
    let pool = SqlitePool::open(path).await?;
    pool.interact(|conn| ensure_schema(conn)).await?;
    Ok(pool)
}

/// Create all three hourly tables + their hour indexes (`CREATE … IF NOT EXISTS`,
/// idempotent). Shared with the read layer so a `traffic_query` reader can open a
/// not-yet-flushed DB without hitting `no such table`.
pub(crate) fn ensure_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(RELAY_SCHEMA)?;
    conn.execute_batch(PUSH_SCHEMA)?;
    conn.execute_batch(IP_SCHEMA)?;
    for index in HOUR_INDEXES {
        conn.execute_batch(index)?;
    }
    Ok(())
}

/// Delete hourly rows older than `retention_days` from every ledger (best-effort
/// retention; these are metrics, so a plain `DELETE` is correct — not session data).
async fn prune(pool: &SqlitePool, retention_days: u64) -> Result<(), SqliteError> {
    // Clamp to ≥1 day: a `0` (a plausible "disable retention" mistake) would make
    // the cutoff `now` and delete even the current hour's rows.
    let cutoff = format!("-{} days", retention_days.max(1));
    pool.interact(move |conn| {
        for table in [RELAY_TRAFFIC_TABLE, PUSH_TRAFFIC_TABLE, IP_TRAFFIC_TABLE] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE hour < datetime('now', ?1)"),
                params![cutoff],
            )?;
        }
        Ok(())
    })
    .await
}

/// Collect both registries, persist the deltas, and only on a durable write advance
/// the baselines + evict. With no connection, advance + evict anyway (so eviction
/// keeps the maps bounded) while discarding the deltas.
async fn flush_once(
    pool: Option<&SqlitePool>,
    relay: &TrafficRegistry,
    push: Option<&PushTrafficRegistry>,
    ip: &IpTrafficRegistry,
) {
    let relay_deltas = relay.collect();
    let push_deltas = push.map(|p| p.collect()).unwrap_or_default();
    let ip_deltas = ip.collect();
    match pool {
        Some(p) => match write_deltas(p, &relay_deltas, &push_deltas, &ip_deltas).await {
            Ok(()) => {
                relay.commit();
                if let Some(p) = push {
                    p.commit();
                }
                ip.commit();
                if !relay_deltas.is_empty() || !push_deltas.is_empty() || !ip_deltas.is_empty() {
                    tracing::debug!(
                        relay = relay_deltas.len(),
                        push = push_deltas.len(),
                        ip = ip_deltas.len(),
                        "remote-host: traffic flushed"
                    );
                }
            }
            // Leave the baselines un-advanced so the same bytes retry next interval.
            Err(e) => tracing::error!(
                error = %e,
                "remote-host: traffic flush write failed; retrying next interval"
            ),
        },
        None => {
            relay.commit();
            if let Some(p) = push {
                p.commit();
            }
            ip.commit();
        }
    }
}

/// Write every delta in **one transaction** (all-or-nothing, so a mid-batch error
/// can't leave the baselines and the DB disagreeing). A no-op when both are empty.
async fn write_deltas(
    pool: &SqlitePool,
    relay: &[remote_host_relay::RelayTrafficDelta],
    push: &[remote_host_push::PushTrafficDelta],
    ip: &[remote_host_edge::IpTrafficDelta],
) -> Result<(), SqliteError> {
    if relay.is_empty() && push.is_empty() && ip.is_empty() {
        return Ok(());
    }
    // The write runs on a blocking thread and so must own its bind values.
    let relay: Vec<(String, String, i64, i64, i64, i64)> = relay
        .iter()
        .map(|d| {
            (
                d.remote_api_key.clone(),
                d.server_id.clone(),
                d.counts.bytes_up as i64,
                d.counts.bytes_down as i64,
                d.counts.frames_up as i64,
                d.counts.frames_down as i64,
            )
        })
        .collect();
    let push: Vec<(String, i64, i64)> = push
        .iter()
        .map(|d| {
            (
                d.device_id.clone(),
                d.counts.sends as i64,
                d.counts.bytes as i64,
            )
        })
        .collect();
    let ip: Vec<(String, String, i64, i64)> = ip
        .iter()
        .map(|d| {
            (
                d.ip.clone(),
                d.endpoint.clone(),
                d.counts.requests as i64,
                d.counts.bytes as i64,
            )
        })
        .collect();
    pool.interact(move |conn| {
        let tx = conn.transaction()?;
        for (remote_api_key, server_id, bytes_up, bytes_down, frames_up, frames_down) in &relay {
            tx.execute(
                RELAY_UPSERT.as_str(),
                params![
                    remote_api_key,
                    server_id,
                    bytes_up,
                    bytes_down,
                    frames_up,
                    frames_down
                ],
            )?;
        }
        for (device_id, sends, bytes) in &push {
            tx.execute(PUSH_UPSERT.as_str(), params![device_id, sends, bytes])?;
        }
        for (addr, endpoint, requests, bytes) in &ip {
            tx.execute(IP_UPSERT.as_str(), params![addr, endpoint, requests, bytes])?;
        }
        tx.commit()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_host_edge::{IpCounts, IpTrafficDelta};
    use remote_host_push::{PushCounts, PushTrafficDelta};
    use remote_host_relay::{Counts, RelayTrafficDelta};

    /// A temp on-disk DB path that removes the `.db`/`-wal`/`-shm` sidecars on drop.
    /// The ledger is pooled, so `:memory:` is not usable in a test: each pooled
    /// connection would get its own private, empty in-memory DB.
    struct TempDbPath(String);
    impl TempDbPath {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .join(format!("traffic-{}.db", rand::random::<u64>()))
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }
    impl Drop for TempDbPath {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0));
            }
        }
    }

    async fn temp_db() -> (TempDbPath, SqlitePool) {
        let path = TempDbPath::new();
        let pool = init_db(&path.0).await.unwrap();
        (path, pool)
    }

    fn relay_delta(key: &str, c: Counts) -> RelayTrafficDelta {
        RelayTrafficDelta {
            remote_api_key: key.into(),
            server_id: "srv".into(),
            counts: c,
        }
    }

    /// Sum a key's counts across all hour buckets (a test's writes share one hour,
    /// but summing is robust to a write landing either side of an hour boundary).
    async fn relay_row(pool: &SqlitePool, key: &str) -> (i64, i64, i64, i64) {
        let key = key.to_string();
        pool.interact(move |conn| {
            conn.query_row(
                "SELECT coalesce(sum(bytes_up),0), coalesce(sum(bytes_down),0), \
                 coalesce(sum(frames_up),0), coalesce(sum(frames_down),0) FROM relay_traffic \
                 WHERE remote_api_key = ?1 AND server_id = 'srv'",
                params![key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
        })
        .await
        .unwrap()
    }

    async fn scalar(pool: &SqlitePool, sql: &'static str) -> i64 {
        pool.interact(move |conn| conn.query_row(sql, [], |row| row.get(0)))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn relay_upsert_accumulates_across_flushes() {
        let (_path, pool) = temp_db().await;
        let first = vec![relay_delta(
            "k",
            Counts {
                bytes_up: 100,
                bytes_down: 10,
                frames_up: 2,
                frames_down: 1,
            },
        )];
        write_deltas(&pool, &first, &[], &[]).await.unwrap();
        // A second flush adds onto the same row (the restart/accumulate property),
        // it does not overwrite.
        let second = vec![relay_delta(
            "k",
            Counts {
                bytes_up: 50,
                bytes_down: 5,
                frames_up: 1,
                frames_down: 0,
            },
        )];
        write_deltas(&pool, &second, &[], &[]).await.unwrap();
        assert_eq!(relay_row(&pool, "k").await, (150, 15, 3, 1));
    }

    #[tokio::test]
    async fn push_upsert_accumulates_and_coexists_in_one_transaction() {
        let (_path, pool) = temp_db().await;
        let relay = vec![relay_delta(
            "k",
            Counts {
                bytes_up: 1,
                bytes_down: 0,
                frames_up: 1,
                frames_down: 0,
            },
        )];
        let push = vec![PushTrafficDelta {
            device_id: "dev".into(),
            counts: PushCounts {
                sends: 3,
                bytes: 900,
            },
        }];
        write_deltas(&pool, &relay, &push, &[]).await.unwrap();
        write_deltas(&pool, &[], &push, &[]).await.unwrap(); // push-only second flush

        assert_eq!(
            scalar(
                &pool,
                "SELECT coalesce(sum(sends),0) FROM push_traffic WHERE device_id = 'dev'"
            )
            .await,
            6,
            "sends accumulate"
        );
        assert_eq!(
            scalar(
                &pool,
                "SELECT coalesce(sum(bytes),0) FROM push_traffic WHERE device_id = 'dev'"
            )
            .await,
            1800,
            "bytes accumulate"
        );
        // The relay row from the first (combined) flush is intact.
        assert_eq!(relay_row(&pool, "k").await, (1, 0, 1, 0));
    }

    #[tokio::test]
    async fn ip_upsert_accumulates_requests_and_bytes() {
        let (_path, pool) = temp_db().await;
        let d = vec![IpTrafficDelta {
            ip: "203.0.113.5".into(),
            endpoint: "content/join".into(),
            counts: IpCounts {
                requests: 1,
                bytes: 1000,
            },
        }];
        write_deltas(&pool, &[], &[], &d).await.unwrap();
        write_deltas(&pool, &[], &[], &d).await.unwrap();
        assert_eq!(
            scalar(
                &pool,
                "SELECT coalesce(sum(requests),0) FROM ip_traffic \
                 WHERE ip = '203.0.113.5' AND endpoint = 'content/join'"
            )
            .await,
            2,
            "requests accumulate"
        );
        assert_eq!(
            scalar(
                &pool,
                "SELECT coalesce(sum(bytes),0) FROM ip_traffic \
                 WHERE ip = '203.0.113.5' AND endpoint = 'content/join'"
            )
            .await,
            2000,
            "bytes accumulate"
        );
    }

    #[tokio::test]
    async fn empty_deltas_write_nothing() {
        let (_path, pool) = temp_db().await;
        write_deltas(&pool, &[], &[], &[]).await.unwrap();
        assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM relay_traffic").await, 0);
    }

    #[tokio::test]
    async fn prune_deletes_rows_past_retention() {
        let (_path, pool) = temp_db().await;
        // One ancient-hour row and one current-hour row, inserted directly.
        pool.interact(|conn| {
            conn.execute_batch(
                "INSERT INTO ip_traffic(ip, endpoint, hour, requests, bytes) \
                 VALUES('1.1.1.1','notify','2000-01-01 00:00:00', 5, 50); \
                 INSERT INTO ip_traffic(ip, endpoint, hour, requests, bytes) \
                 VALUES('1.1.1.1','notify', strftime('%Y-%m-%d %H:00:00','now'), 1, 10);",
            )
        })
        .await
        .unwrap();
        prune(&pool, 60).await.unwrap();
        assert_eq!(
            scalar(&pool, "SELECT count(*) FROM ip_traffic").await,
            1,
            "the >60-day-old hour is pruned; the current one is kept"
        );
    }
}
