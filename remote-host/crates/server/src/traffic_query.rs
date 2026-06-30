//! Read-only query layer over the durable traffic ledger.
//!
//! The flush task in [`crate::traffic`] is the sole writer; this reader opens its
//! own connection (WAL, so it never blocks the writer) and runs every aggregate
//! the dashboard's Traffic / Devices pages need: per-bucket time series, top-N by
//! bytes, and the overview last-hour / last-24h totals.
//!
//! Two safety invariants hold throughout:
//! * the time window is always a **bound modifier** (`datetime('now', ?1)`) — never
//!   `format!`'d into SQL — so an attacker-influenced range can't reach the query;
//! * only **trusted consts** (table names from [`crate::traffic`], the bucket
//!   expression, [`CURRENT_HOUR_EXPR`]) are ever interpolated.
//!
//! Masking is NOT done here: top-N queries return the raw `remote_api_key` /
//! `device_id` / `ip`; the backend masks (or drops) the secret before any DTO is
//! serialized.

use libsql::{Builder, Connection, Database, params};
use remote_host_dashboard::Range;

use crate::traffic::{IP_TRAFFIC_TABLE, PUSH_TRAFFIC_TABLE, RELAY_TRAFFIC_TABLE, ensure_schema};

/// `datetime('now', ?1)` window modifiers, bound (never interpolated).
const WINDOW_24H: &str = "-24 hours";
const WINDOW_7D: &str = "-7 days";
const WINDOW_30D: &str = "-30 days";
const WINDOW_60D: &str = "-60 days";

/// Bucket grouping expressions (trusted consts, interpolated). Hourly keeps the
/// full `YYYY-MM-DD HH:00:00` stamp; daily collapses to the `YYYY-MM-DD` date.
const BUCKET_HOURLY: &str = "hour";
const BUCKET_DAILY: &str = "substr(hour, 1, 10)";

/// The window modifier for a range (bound as `?1`).
fn window(r: Range) -> &'static str {
    match r {
        Range::H24 => WINDOW_24H,
        Range::D7 => WINDOW_7D,
        Range::D30 => WINDOW_30D,
        Range::D60 => WINDOW_60D,
    }
}

/// The bucket grouping expression for a range: hourly for ≤7d, daily beyond.
fn bucket_expr(r: Range) -> &'static str {
    match r {
        Range::H24 | Range::D7 => BUCKET_HOURLY,
        Range::D30 | Range::D60 => BUCKET_DAILY,
    }
}

/// SQLite `coalesce(sum(...),0)` columns are non-negative `INTEGER`s; read them as
/// `i64` and clamp to `u64` (a negative can only mean corruption — treat as 0).
fn nonneg(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TrafficQueryError {
    #[error("open traffic read db at {path}: {source}")]
    Open { path: String, source: libsql::Error },
    #[error("traffic query failed: {0}")]
    Query(#[from] libsql::Error),
}

// --- Reader-local row shapes (the backend maps + masks these into the DTOs) ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayBucketRow {
    pub bucket: String,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub frames_up: u64,
    pub frames_down: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayKeyRow {
    pub remote_api_key: String,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub frames_up: u64,
    pub frames_down: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushBucketRow {
    pub bucket: String,
    pub sends: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushDeviceRow {
    pub device_id: String,
    pub sends: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IpBucketRow {
    pub bucket: String,
    pub requests: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IpEndpointRow {
    pub endpoint: String,
    pub requests: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IpSourceRow {
    pub ip: String,
    pub requests: u64,
    pub bytes: u64,
}

pub(crate) struct TrafficReader {
    // Held only to keep the connection valid for the reader's lifetime.
    _db: Database,
    conn: Connection,
}

impl TrafficReader {
    /// Open the ledger read-only. An empty `path` means `TRAFFIC_DB_PATH` is unset
    /// (persistence off) → `Ok(None)`, so the backend degrades to empty stats. The
    /// `busy_timeout` and WAL pragmas mirror the writer; [`ensure_schema`] guards
    /// against a read before the first flush has created the tables.
    pub(crate) async fn open(path: &str) -> Result<Option<Self>, TrafficQueryError> {
        if path.is_empty() {
            return Ok(None);
        }
        let db = Builder::new_local(path)
            .build()
            .await
            .map_err(|e| TrafficQueryError::Open {
                path: path.to_owned(),
                source: e,
            })?;
        let conn = db.connect().map_err(|e| TrafficQueryError::Open {
            path: path.to_owned(),
            source: e,
        })?;
        let mut r = conn.query("PRAGMA journal_mode=WAL", ()).await?;
        let _ = r.next().await?;
        // `PRAGMA busy_timeout=N` echoes the new value as a row in this libsql
        // build, so drain it rather than `execute` (which would error).
        let mut r = conn.query("PRAGMA busy_timeout=5000", ()).await?;
        let _ = r.next().await?;
        ensure_schema(&conn).await?;
        Ok(Some(Self { _db: db, conn }))
    }

    // --- Per-bucket time series ---

    pub(crate) async fn relay_series(
        &self,
        range: Range,
    ) -> Result<Vec<RelayBucketRow>, TrafficQueryError> {
        let sql = format!(
            "SELECT {bucket} AS bucket, coalesce(sum(bytes_up),0), coalesce(sum(bytes_down),0), \
             coalesce(sum(frames_up),0), coalesce(sum(frames_down),0) \
             FROM {RELAY_TRAFFIC_TABLE} WHERE hour >= datetime('now', ?1) \
             GROUP BY bucket ORDER BY bucket",
            bucket = bucket_expr(range),
        );
        let mut rows = self.conn.query(&sql, params![window(range)]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(RelayBucketRow {
                bucket: row.get::<String>(0)?,
                bytes_up: nonneg(row.get::<i64>(1)?),
                bytes_down: nonneg(row.get::<i64>(2)?),
                frames_up: nonneg(row.get::<i64>(3)?),
                frames_down: nonneg(row.get::<i64>(4)?),
            });
        }
        Ok(out)
    }

    pub(crate) async fn push_series(
        &self,
        range: Range,
    ) -> Result<Vec<PushBucketRow>, TrafficQueryError> {
        let sql = format!(
            "SELECT {bucket} AS bucket, coalesce(sum(sends),0), coalesce(sum(bytes),0) \
             FROM {PUSH_TRAFFIC_TABLE} WHERE hour >= datetime('now', ?1) \
             GROUP BY bucket ORDER BY bucket",
            bucket = bucket_expr(range),
        );
        let mut rows = self.conn.query(&sql, params![window(range)]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(PushBucketRow {
                bucket: row.get::<String>(0)?,
                sends: nonneg(row.get::<i64>(1)?),
                bytes: nonneg(row.get::<i64>(2)?),
            });
        }
        Ok(out)
    }

    pub(crate) async fn ip_series(
        &self,
        range: Range,
    ) -> Result<Vec<IpBucketRow>, TrafficQueryError> {
        let sql = format!(
            "SELECT {bucket} AS bucket, coalesce(sum(requests),0), coalesce(sum(bytes),0) \
             FROM {IP_TRAFFIC_TABLE} WHERE hour >= datetime('now', ?1) \
             GROUP BY bucket ORDER BY bucket",
            bucket = bucket_expr(range),
        );
        let mut rows = self.conn.query(&sql, params![window(range)]).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(IpBucketRow {
                bucket: row.get::<String>(0)?,
                requests: nonneg(row.get::<i64>(1)?),
                bytes: nonneg(row.get::<i64>(2)?),
            });
        }
        Ok(out)
    }

    // --- Top-N by bytes (raw identifiers; backend masks) ---

    pub(crate) async fn relay_top_keys(
        &self,
        range: Range,
        limit: u32,
    ) -> Result<Vec<RelayKeyRow>, TrafficQueryError> {
        let sql = format!(
            "SELECT remote_api_key, coalesce(sum(bytes_up),0) AS up, \
             coalesce(sum(bytes_down),0) AS down, coalesce(sum(frames_up),0), \
             coalesce(sum(frames_down),0) \
             FROM {RELAY_TRAFFIC_TABLE} WHERE hour >= datetime('now', ?1) \
             GROUP BY remote_api_key ORDER BY (up + down) DESC LIMIT ?2"
        );
        let mut rows = self
            .conn
            .query(&sql, params![window(range), i64::from(limit)])
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(RelayKeyRow {
                remote_api_key: row.get::<String>(0)?,
                bytes_up: nonneg(row.get::<i64>(1)?),
                bytes_down: nonneg(row.get::<i64>(2)?),
                frames_up: nonneg(row.get::<i64>(3)?),
                frames_down: nonneg(row.get::<i64>(4)?),
            });
        }
        Ok(out)
    }

    pub(crate) async fn push_top_devices(
        &self,
        range: Range,
        limit: u32,
    ) -> Result<Vec<PushDeviceRow>, TrafficQueryError> {
        let sql = format!(
            "SELECT device_id, coalesce(sum(sends),0), coalesce(sum(bytes),0) AS b \
             FROM {PUSH_TRAFFIC_TABLE} WHERE hour >= datetime('now', ?1) \
             GROUP BY device_id ORDER BY b DESC LIMIT ?2"
        );
        let mut rows = self
            .conn
            .query(&sql, params![window(range), i64::from(limit)])
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(PushDeviceRow {
                device_id: row.get::<String>(0)?,
                sends: nonneg(row.get::<i64>(1)?),
                bytes: nonneg(row.get::<i64>(2)?),
            });
        }
        Ok(out)
    }

    pub(crate) async fn ip_top_endpoints(
        &self,
        range: Range,
        limit: u32,
    ) -> Result<Vec<IpEndpointRow>, TrafficQueryError> {
        let sql = format!(
            "SELECT endpoint, coalesce(sum(requests),0), coalesce(sum(bytes),0) AS b \
             FROM {IP_TRAFFIC_TABLE} WHERE hour >= datetime('now', ?1) \
             GROUP BY endpoint ORDER BY b DESC LIMIT ?2"
        );
        let mut rows = self
            .conn
            .query(&sql, params![window(range), i64::from(limit)])
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(IpEndpointRow {
                endpoint: row.get::<String>(0)?,
                requests: nonneg(row.get::<i64>(1)?),
                bytes: nonneg(row.get::<i64>(2)?),
            });
        }
        Ok(out)
    }

    pub(crate) async fn ip_top_sources(
        &self,
        range: Range,
        limit: u32,
    ) -> Result<Vec<IpSourceRow>, TrafficQueryError> {
        let sql = format!(
            "SELECT ip, coalesce(sum(requests),0), coalesce(sum(bytes),0) AS b \
             FROM {IP_TRAFFIC_TABLE} WHERE hour >= datetime('now', ?1) \
             GROUP BY ip ORDER BY b DESC LIMIT ?2"
        );
        let mut rows = self
            .conn
            .query(&sql, params![window(range), i64::from(limit)])
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(IpSourceRow {
                ip: row.get::<String>(0)?,
                requests: nonneg(row.get::<i64>(1)?),
                bytes: nonneg(row.get::<i64>(2)?),
            });
        }
        Ok(out)
    }

    /// One source IP's per-endpoint totals over the window. `ip` is **bound** (`?2`),
    /// never interpolated — it arrives from the operator via the request path. The
    /// endpoint label space is the fixed route set, so `limit` only guards corruption.
    pub(crate) async fn ip_endpoints(
        &self,
        ip: &str,
        range: Range,
        limit: u32,
    ) -> Result<Vec<IpEndpointRow>, TrafficQueryError> {
        let sql = format!(
            "SELECT endpoint, coalesce(sum(requests),0), coalesce(sum(bytes),0) AS b \
             FROM {IP_TRAFFIC_TABLE} WHERE hour >= datetime('now', ?1) AND ip = ?2 \
             GROUP BY endpoint ORDER BY b DESC LIMIT ?3"
        );
        let mut rows = self
            .conn
            .query(&sql, params![window(range), ip, i64::from(limit)])
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(IpEndpointRow {
                endpoint: row.get::<String>(0)?,
                requests: nonneg(row.get::<i64>(1)?),
                bytes: nonneg(row.get::<i64>(2)?),
            });
        }
        Ok(out)
    }

    /// All-time per-device send/byte totals (no window, no `LIMIT`), keyed by
    /// `device_id` — the dashboard's Devices page joins these onto the live
    /// bindings so each device shows its cumulative push volume.
    pub(crate) async fn push_device_totals(
        &self,
    ) -> Result<std::collections::HashMap<String, (u64, u64)>, TrafficQueryError> {
        let sql = format!(
            "SELECT device_id, coalesce(sum(sends),0), coalesce(sum(bytes),0) \
             FROM {PUSH_TRAFFIC_TABLE} GROUP BY device_id"
        );
        let mut rows = self.conn.query(&sql, ()).await?;
        let mut out = std::collections::HashMap::new();
        while let Some(row) = rows.next().await? {
            out.insert(
                row.get::<String>(0)?,
                (nonneg(row.get::<i64>(1)?), nonneg(row.get::<i64>(2)?)),
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn reader() -> TrafficReader {
        TrafficReader::open(":memory:")
            .await
            .unwrap()
            .expect(":memory: is non-empty so open yields Some")
    }

    /// Insert a relay row stamped `hours_ago` hours before now (truncated to the
    /// hour by the same `strftime` the writer uses).
    async fn seed_relay(r: &TrafficReader, key: &str, hours_ago: i64, up: i64, down: i64) {
        r.conn
            .execute(
                "INSERT INTO relay_traffic(remote_api_key, server_id, hour, bytes_up, bytes_down, \
                 frames_up, frames_down) VALUES(?1, 'srv', \
                 strftime('%Y-%m-%d %H:00:00','now', ?2), ?3, ?4, 1, 1)",
                params![key, format!("-{hours_ago} hours"), up, down],
            )
            .await
            .unwrap();
    }

    async fn seed_ip(
        r: &TrafficReader,
        ip: &str,
        endpoint: &str,
        hours_ago: i64,
        reqs: i64,
        bytes: i64,
    ) {
        r.conn
            .execute(
                "INSERT INTO ip_traffic(ip, endpoint, hour, requests, bytes) VALUES(?1, ?2, \
                 strftime('%Y-%m-%d %H:00:00','now', ?3), ?4, ?5)",
                params![ip, endpoint, format!("-{hours_ago} hours"), reqs, bytes],
            )
            .await
            .unwrap();
    }

    async fn seed_push(r: &TrafficReader, device: &str, hours_ago: i64, sends: i64, bytes: i64) {
        r.conn
            .execute(
                "INSERT INTO push_traffic(device_id, hour, sends, bytes) VALUES(?1, \
                 strftime('%Y-%m-%d %H:00:00','now', ?2), ?3, ?4)",
                params![device, format!("-{hours_ago} hours"), sends, bytes],
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn open_empty_path_is_none() {
        assert!(TrafficReader::open("").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_db_yields_empty_and_zeroed() {
        let r = reader().await;
        assert!(r.relay_series(Range::H24).await.unwrap().is_empty());
        assert!(r.relay_top_keys(Range::H24, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn range_filters_across_the_window_boundary() {
        let r = reader().await;
        seed_relay(&r, "k", 1, 100, 10).await; // inside 24h
        seed_relay(&r, "k", 48, 200, 20).await; // outside 24h, inside 7d
        let h24: u64 = r
            .relay_series(Range::H24)
            .await
            .unwrap()
            .iter()
            .map(|b| b.bytes_up)
            .sum();
        assert_eq!(h24, 100, "the 48h-old row is outside the 24h window");
        let d7: u64 = r
            .relay_series(Range::D7)
            .await
            .unwrap()
            .iter()
            .map(|b| b.bytes_up)
            .sum();
        assert_eq!(d7, 300, "both rows are inside the 7d window");
    }

    #[tokio::test]
    async fn daily_buckets_collapse_same_day_hours() {
        let r = reader().await;
        // Two rows a few hours apart but (assuming the test doesn't straddle
        // midnight UTC) within the same calendar day.
        seed_relay(&r, "k", 1, 10, 0).await;
        seed_relay(&r, "k", 3, 20, 0).await;
        let hourly = r.relay_series(Range::D7).await.unwrap();
        let daily = r.relay_series(Range::D30).await.unwrap();
        // Hourly keeps them separate (2 buckets); daily collapses to 1 — unless the
        // 3h gap crossed midnight, in which case both views agree at 2.
        assert!(hourly.len() >= daily.len());
        assert!(daily.len() <= 2);
        let daily_total: u64 = daily.iter().map(|b| b.bytes_up).sum();
        assert_eq!(daily_total, 30);
    }

    #[tokio::test]
    async fn top_n_orders_by_bytes_and_truncates() {
        let r = reader().await;
        seed_relay(&r, "small", 1, 1, 1).await;
        seed_relay(&r, "big", 1, 1000, 1000).await;
        seed_relay(&r, "mid", 1, 100, 100).await;
        let top = r.relay_top_keys(Range::H24, 2).await.unwrap();
        assert_eq!(top.len(), 2, "LIMIT truncates to 2");
        assert_eq!(top[0].remote_api_key, "big");
        assert_eq!(top[1].remote_api_key, "mid");
    }

    #[tokio::test]
    async fn ip_top_endpoints_and_sources() {
        let r = reader().await;
        seed_ip(&r, "1.1.1.1", "content/join", 1, 5, 500).await;
        seed_ip(&r, "2.2.2.2", "notify", 1, 2, 50).await;
        let by_ep = r.ip_top_endpoints(Range::H24, 10).await.unwrap();
        assert_eq!(by_ep[0].endpoint, "content/join");
        assert_eq!(by_ep[0].bytes, 500);
        let by_ip = r.ip_top_sources(Range::H24, 10).await.unwrap();
        assert_eq!(by_ip[0].ip, "1.1.1.1");
    }

    #[tokio::test]
    async fn ip_endpoints_filters_to_one_source_and_orders_by_bytes() {
        let r = reader().await;
        seed_ip(&r, "1.1.1.1", "content/join", 1, 5, 500).await;
        seed_ip(&r, "1.1.1.1", "control", 1, 9, 90).await;
        seed_ip(&r, "2.2.2.2", "notify", 1, 2, 50).await; // a different IP, must be excluded
        let eps = r.ip_endpoints("1.1.1.1", Range::H24, 10).await.unwrap();
        assert_eq!(eps.len(), 2, "only 1.1.1.1's two endpoints");
        assert_eq!(eps[0].endpoint, "content/join", "ordered by bytes desc");
        assert_eq!(eps[0].bytes, 500);
        assert_eq!(eps[1].endpoint, "control");
        assert!(
            !eps.iter().any(|e| e.endpoint == "notify"),
            "the other IP's endpoint is filtered out"
        );
    }

    /// The push ledger: a windowed series + top-devices ordering, and the all-time
    /// `push_device_totals` (no window) the Devices page joins onto live bindings.
    #[tokio::test]
    async fn push_series_top_devices_and_all_time_totals() {
        let r = reader().await;
        seed_push(&r, "dev-a", 1, 3, 300).await; // inside 24h
        seed_push(&r, "dev-a", 100, 5, 5000).await; // older — only in all-time totals
        seed_push(&r, "dev-b", 2, 7, 70).await;

        // Windowed series: only the two rows inside 24h contribute.
        let series_24h: u64 = r
            .push_series(Range::H24)
            .await
            .unwrap()
            .iter()
            .map(|b| b.bytes)
            .sum();
        assert_eq!(series_24h, 370, "dev-a 300 + dev-b 70 within 24h");

        // Top-N ranks by bytes within the window: dev-a (300) over dev-b (70).
        let top = r.push_top_devices(Range::H24, 1).await.unwrap();
        assert_eq!(top.len(), 1, "LIMIT truncates");
        assert_eq!(top[0].device_id, "dev-a");
        assert_eq!(top[0].bytes, 300);

        // All-time totals ignore the window: dev-a sums both its rows.
        let totals = r.push_device_totals().await.unwrap();
        assert_eq!(totals.get("dev-a"), Some(&(8u64, 5300u64)));
        assert_eq!(totals.get("dev-b"), Some(&(7u64, 70u64)));
    }
}
