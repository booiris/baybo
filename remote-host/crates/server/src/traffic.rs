//! The durable traffic ledger.
//!
//! A single background task drains the relay's per-`(remote_api_key, server_id)`
//! and push's per-`device_id` in-memory counters on an interval and
//! **accumulates** them into a SQLite (libsql) DB — a row's stored total is grown
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

use std::sync::Arc;
use std::time::Duration;

use libsql::{Builder, Connection, Database, params};
use remote_host_push::PushTrafficRegistry;
use remote_host_relay::TrafficRegistry;

/// One row per `(remote_api_key, server_id)`; the PRIMARY KEY is the UPSERT
/// conflict target. Counts are lifetime cumulative.
const RELAY_SCHEMA: &str = r#"CREATE TABLE IF NOT EXISTS relay_traffic (
    remote_api_key TEXT NOT NULL,
    server_id TEXT NOT NULL,
    bytes_up INTEGER NOT NULL DEFAULT 0,
    bytes_down INTEGER NOT NULL DEFAULT 0,
    frames_up INTEGER NOT NULL DEFAULT 0,
    frames_down INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (remote_api_key, server_id)
)"#;

/// One row per `device_id`: cumulative APNs sends + payload bytes egressed.
const PUSH_SCHEMA: &str = r#"CREATE TABLE IF NOT EXISTS push_traffic (
    device_id TEXT PRIMARY KEY,
    sends INTEGER NOT NULL DEFAULT 0,
    bytes INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
)"#;

/// Add an interval's relay delta onto the row's lifetime total (insert the row on
/// first sight). `excluded.*` is the just-inserted delta.
const RELAY_UPSERT: &str = r#"INSERT INTO relay_traffic
    (remote_api_key, server_id, bytes_up, bytes_down, frames_up, frames_down, updated_at)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))
ON CONFLICT(remote_api_key, server_id) DO UPDATE SET
    bytes_up = bytes_up + excluded.bytes_up,
    bytes_down = bytes_down + excluded.bytes_down,
    frames_up = frames_up + excluded.frames_up,
    frames_down = frames_down + excluded.frames_down,
    updated_at = excluded.updated_at"#;

/// Add an interval's push delta onto the device's lifetime total.
const PUSH_UPSERT: &str = r#"INSERT INTO push_traffic (device_id, sends, bytes, updated_at)
    VALUES (?1, ?2, ?3, datetime('now'))
ON CONFLICT(device_id) DO UPDATE SET
    sends = sends + excluded.sends,
    bytes = bytes + excluded.bytes,
    updated_at = excluded.updated_at"#;

/// Spawn the flush task. `db_path` empty ⇒ no persistence (drain-and-discard, still
/// evicts). `flush_secs` is clamped to ≥1. The relay registry is required (the relay
/// is always on); the push one is `Some` only when the push role is mounted.
/// `relay_max_tracked` recomputes the relay entry cap (sized to the live admission
/// connection capacity) — it is applied before the first flush and re-applied each
/// interval so the cap follows hot-reloaded admission edits.
pub(crate) fn spawn<F>(
    db_path: String,
    flush_secs: u64,
    relay: Arc<TrafficRegistry>,
    push: Option<Arc<PushTrafficRegistry>>,
    relay_max_tracked: F,
) where
    F: Fn() -> usize + Send + 'static,
{
    tokio::spawn(async move {
        let secs = flush_secs.max(1);
        // Hold the Database for the loop's whole life so the connection stays valid.
        let opened = if db_path.is_empty() {
            tracing::info!(
                "remote-host: traffic persistence disabled (empty TRAFFIC_DB_PATH); in-memory accounting + eviction only"
            );
            None
        } else {
            match init_db(&db_path).await {
                Ok(pair) => {
                    tracing::info!(path = %db_path, period_secs = secs, "remote-host: traffic ledger enabled");
                    Some(pair)
                }
                Err(e) => {
                    tracing::error!(path = %db_path, error = %e, "remote-host: traffic DB open failed; in-memory eviction only");
                    None
                }
            }
        };
        let conn = opened.as_ref().map(|(_db, conn)| conn);
        // Size the cap to the current admission capacity before the first leg can
        // connect; the loop keeps it fresh as admission hot-reloads.
        relay.set_max_tracked(relay_max_tracked());
        let mut tick = tokio::time::interval(Duration::from_secs(secs));
        tick.tick().await; // the first tick is immediate; nothing to flush yet
        loop {
            tick.tick().await;
            let cap = relay_max_tracked();
            relay.set_max_tracked(cap);
            flush_once(conn, &relay, push.as_deref()).await;
            // Surface saturation once per interval (never per meter_for): at the cap,
            // new (remote_api_key, server_id) pairs are recorded by an inert meter.
            if relay.tracked_len() >= cap {
                tracing::warn!(
                    cap,
                    "remote-host: relay traffic map at its capacity cap; new (remote_api_key, server_id) pairs are going unrecorded — likely relay_node_id churn or an undersized admission max_conns sum"
                );
            }
        }
    });
}

/// Open the local DB and ensure both tables.
async fn init_db(path: &str) -> Result<(Database, Connection), libsql::Error> {
    let db = Builder::new_local(path).build().await?;
    let conn = db.connect()?;
    conn.execute(RELAY_SCHEMA, ()).await?;
    conn.execute(PUSH_SCHEMA, ()).await?;
    Ok((db, conn))
}

/// Collect both registries, persist the deltas, and only on a durable write advance
/// the baselines + evict. With no connection, advance + evict anyway (so eviction
/// keeps the maps bounded) while discarding the deltas.
async fn flush_once(
    conn: Option<&Connection>,
    relay: &TrafficRegistry,
    push: Option<&PushTrafficRegistry>,
) {
    let relay_deltas = relay.collect();
    let push_deltas = push.map(|p| p.collect()).unwrap_or_default();
    match conn {
        Some(c) => match write_deltas(c, &relay_deltas, &push_deltas).await {
            Ok(()) => {
                relay.commit();
                if let Some(p) = push {
                    p.commit();
                }
                if !relay_deltas.is_empty() || !push_deltas.is_empty() {
                    tracing::debug!(
                        relay = relay_deltas.len(),
                        push = push_deltas.len(),
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
        }
    }
}

/// Write every delta in **one transaction** (all-or-nothing, so a mid-batch error
/// can't leave the baselines and the DB disagreeing). A no-op when both are empty.
async fn write_deltas(
    conn: &Connection,
    relay: &[remote_host_relay::RelayTrafficDelta],
    push: &[remote_host_push::PushTrafficDelta],
) -> Result<(), libsql::Error> {
    if relay.is_empty() && push.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction().await?;
    let result = async {
        for d in relay {
            tx.execute(
                RELAY_UPSERT,
                params![
                    d.remote_api_key.clone(),
                    d.server_id.clone(),
                    d.counts.bytes_up as i64,
                    d.counts.bytes_down as i64,
                    d.counts.frames_up as i64,
                    d.counts.frames_down as i64,
                ],
            )
            .await?;
        }
        for d in push {
            tx.execute(
                PUSH_UPSERT,
                params![
                    d.device_id.clone(),
                    d.counts.sends as i64,
                    d.counts.bytes as i64,
                ],
            )
            .await?;
        }
        Ok::<(), libsql::Error>(())
    }
    .await;
    match result {
        Ok(()) => tx.commit().await,
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_host_push::{PushCounts, PushTrafficDelta};
    use remote_host_relay::{Counts, RelayTrafficDelta};

    async fn mem_db() -> (Database, Connection) {
        init_db(":memory:").await.unwrap()
    }

    fn relay_delta(key: &str, c: Counts) -> RelayTrafficDelta {
        RelayTrafficDelta {
            remote_api_key: key.into(),
            server_id: "srv".into(),
            counts: c,
        }
    }

    async fn relay_row(conn: &Connection, key: &str) -> (i64, i64, i64, i64) {
        let mut rows = conn
            .query(
                "SELECT bytes_up, bytes_down, frames_up, frames_down FROM relay_traffic \
                 WHERE remote_api_key = ?1 AND server_id = 'srv'",
                params![key],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        (
            row.get(0).unwrap(),
            row.get(1).unwrap(),
            row.get(2).unwrap(),
            row.get(3).unwrap(),
        )
    }

    #[tokio::test]
    async fn relay_upsert_accumulates_across_flushes() {
        let (_db, conn) = mem_db().await;
        let first = vec![relay_delta(
            "k",
            Counts {
                bytes_up: 100,
                bytes_down: 10,
                frames_up: 2,
                frames_down: 1,
            },
        )];
        write_deltas(&conn, &first, &[]).await.unwrap();
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
        write_deltas(&conn, &second, &[]).await.unwrap();
        assert_eq!(relay_row(&conn, "k").await, (150, 15, 3, 1));
    }

    #[tokio::test]
    async fn push_upsert_accumulates_and_coexists_in_one_transaction() {
        let (_db, conn) = mem_db().await;
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
        write_deltas(&conn, &relay, &push).await.unwrap();
        write_deltas(&conn, &[], &push).await.unwrap(); // push-only second flush

        let mut rows = conn
            .query(
                "SELECT sends, bytes FROM push_traffic WHERE device_id = 'dev'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 6, "sends accumulate");
        assert_eq!(row.get::<i64>(1).unwrap(), 1800, "bytes accumulate");
        // The relay row from the first (combined) flush is intact.
        assert_eq!(relay_row(&conn, "k").await, (1, 0, 1, 0));
    }

    #[tokio::test]
    async fn empty_deltas_write_nothing() {
        let (_db, conn) = mem_db().await;
        write_deltas(&conn, &[], &[]).await.unwrap();
        let mut rows = conn
            .query("SELECT COUNT(*) FROM relay_traffic", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 0);
    }
}
