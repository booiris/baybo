//! sqlite implementation of [`CronStore`].
//!
//! Cron rows persist as the same `(id, user_id, status, next_trigger_at,
//! data)` shape they always have — `data` carries the full
//! [`CronJob`] / [`CronExecution`] as JSON, and the queryable columns
//! are projected out at write time. The translation lives here (not
//! in the cron scheduler) so `baybo-cron` can stay free of `baybo-storage`.

use async_trait::async_trait;
use baybo_model::{CronExecution, CronJob, ExecutionStatus};
use baybo_store::StorageError;
use baybo_store::cron::{CronFire, CronStore, ExecutionCompletion, Result};
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use super::time::{from_us, to_us};

pub struct SqliteCronStore {
    pool: SqlitePool,
}

impl SqliteCronStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

// ── Row helpers ───────────────────────────────────────────────────────
//
// Decoding runs *outside* the connection closure: a `query_map` row
// callback may only fail with a rusqlite error, so the queries below pull
// the raw columns out and hand them here.

fn job_from_row(
    deleted_us: Option<i64>,
    pinned_col: i64,
    builtin_col: i64,
    data: &str,
) -> Result<CronJob> {
    let mut job: CronJob = serde_json::from_str(data)
        .map_err(|e| StorageError::Storage(format!("failed to deserialize cron job: {e}")))?;
    // The row-level `deleted_at` column is the source of truth — `delete` and
    // `restore` own it and `save` never writes it, so a caller holding a
    // snapshot from before a deletion cannot write the job back to life.
    job.deleted_at = match deleted_us {
        Some(us) => Some(from_us(us).ok_or_else(|| {
            StorageError::Storage(format!(
                "cron job {} has an out-of-range deleted_at: {us}",
                job.id
            ))
        })?),
        None => None,
    };
    // Same discipline for `pinned`: the flat column is authoritative,
    // `set_pinned` owns it, `save` never writes it, and it is `#[serde(skip)]`
    // so the blob never carries it either.
    job.pinned = pinned_col != 0;
    // And for `builtin`: written once by the seed, never by `save`, and
    // `#[serde(skip)]` so no blob write can invent it.
    job.builtin = builtin_col != 0;
    Ok(job)
}

fn execution_from_row(status_col: &str, data: &str) -> Result<CronExecution> {
    let mut exec: CronExecution = serde_json::from_str(data)
        .map_err(|e| StorageError::Storage(format!("failed to deserialize execution: {e}")))?;
    // The row-level `status` column is the source of truth — `update_execution_status`
    // touches the column without rewriting `data`.
    exec.status = match status_col {
        "dispatched" => ExecutionStatus::Dispatched,
        _ => ExecutionStatus::Pending,
    };
    Ok(exec)
}

/// Columns every job query selects; [`job_from_row`] takes the `deleted_at`
/// (index 4), `pinned` (index 5), `builtin` (index 6) and `data` (index 7)
/// columns out of that list.
const JOB_COLS: &str = "id, user_id, status, next_trigger_at, deleted_at, pinned, builtin, data";

/// The predicate that keeps recycled jobs out of a query. Applied in SQL — not
/// in the caller — so no listing can forget it.
const LIVE_ONLY: &str = "deleted_at IS NULL";

/// The predicate a fire's write-back matches the stored row against: the row is
/// still live, and still carries the `status` and `next_trigger_at` of the
/// snapshot the fire read. Those are the two columns a fire, a pause, a resume,
/// and a reschedule all move, so a row that still has both is a row whose
/// *schedule* nothing has happened to since the read — which is all a fire
/// needs, since it stamps its four fields onto the stored blob and rewrites
/// nothing a user authored.
///
/// A blob-only write (an edit) needs more than this — see
/// [`CronStore::save_if_unchanged`].
const UNMOVED: &str = "status = :expected_status \
                       AND next_trigger_at = :expected_next_trigger_at \
                       AND deleted_at IS NULL";

/// A snapshot projected onto the columns [`UNMOVED`] tests.
struct Unmoved {
    status: String,
    next_trigger_us: i64,
}

impl From<&CronJob> for Unmoved {
    fn from(job: &CronJob) -> Self {
        Self {
            status: job.status.as_str().to_string(),
            next_trigger_us: job.next_trigger_at.map(to_us).unwrap_or(0),
        }
    }
}

/// The `(deleted_at, pinned, builtin, data)` tuple each job query lifts out
/// of a row.
type TurnRow = (Option<i64>, i64, i64, String);

/// Columns every execution query selects; [`execution_from_row`] takes the
/// `status` (index 5) and `data` (index 6) columns out of that list.
const EXECUTION_COLS: &str = "id, job_id, user_id, scheduled_fire_time, triggered_at, status, data";

/// The `(status, data)` pair each execution query lifts out of a row.
type ExecutionRow = (String, String);

fn execution_status_str(status: ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Pending => "pending",
        ExecutionStatus::Dispatched => "dispatched",
    }
}

/// The `deleted_at` column is the sole source of truth for a job's
/// recycle-bin state (see [`job_from_row`]); keeping it out of the blob is what
/// stops a stale snapshot's copy from ever contradicting the column.
fn serialize_job(job: &CronJob) -> Result<String> {
    let job = CronJob {
        deleted_at: None,
        ..job.clone()
    };
    serde_json::to_string(&job)
        .map_err(|e| StorageError::Storage(format!("failed to serialize cron job: {e}")))
}

fn serialize_execution(exec: &CronExecution) -> Result<String> {
    serde_json::to_string(exec)
        .map_err(|e| StorageError::Storage(format!("failed to serialize execution: {e}")))
}

fn decode_jobs(rows: Vec<TurnRow>) -> Result<Vec<CronJob>> {
    rows.iter()
        .map(|(deleted_us, pinned, builtin, data)| {
            job_from_row(*deleted_us, *pinned, *builtin, data)
        })
        .collect()
}

/// Lift the `(deleted_at, pinned, builtin, data)` tuple out of a row
/// selected with [`JOB_COLS`].
fn job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnRow> {
    Ok((
        row.get::<_, Option<i64>>(4)?,
        row.get::<_, i64>(5)?,
        row.get::<_, i64>(6)?,
        row.get::<_, String>(7)?,
    ))
}

fn decode_executions(rows: Vec<ExecutionRow>) -> Result<Vec<CronExecution>> {
    rows.iter()
        .map(|(status, data)| execution_from_row(status, data))
        .collect()
}

#[async_trait]
impl CronStore for SqliteCronStore {
    async fn create(&self, job: &CronJob) -> Result<()> {
        let data = serialize_job(job)?;
        let next_trigger_us = job.next_trigger_at.map(to_us).unwrap_or(0);
        let deleted_us = job.deleted_at.map(to_us);
        let id = job.id.clone();
        let user_id = job.user_id.clone();
        let status = job.status.as_str().to_string();
        let pinned_flag: i64 = if job.pinned { 1 } else { 0 };
        let builtin_flag: i64 = if job.builtin { 1 } else { 0 };
        self.pool
            .interact_write("cron.create", move |conn| {
                conn.execute(
                    "INSERT INTO cron_jobs (id, user_id, status, next_trigger_at, deleted_at, pinned, builtin, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![id, user_id, status, next_trigger_us, deleted_us, pinned_flag, builtin_flag, data],
                )?;
                Ok(())
            })
            .await
    }

    async fn get(&self, job_id: &str) -> Result<Option<CronJob>> {
        let job_id = job_id.to_string();
        let row: Option<TurnRow> = self
            .pool
            .interact("cron.get", move |conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT {JOB_COLS} FROM cron_jobs WHERE id = ?1"),
                        rusqlite::params![job_id],
                        job_row,
                    )
                    .optional()?)
            })
            .await?;

        match row {
            Some((deleted_us, pinned, builtin, data)) => {
                Ok(Some(job_from_row(deleted_us, pinned, builtin, &data)?))
            }
            None => Ok(None),
        }
    }

    /// Persist everything about a job **except** its recycle-bin state, which
    /// belongs to the `deleted_at` column that [`CronStore::delete`] and
    /// [`CronStore::restore`] own. The tick loop reads a job, works, then
    /// writes it back; leaving the column alone is what stops that write-back
    /// from resurrecting a job the user deleted in between.
    async fn save(&self, job: &CronJob) -> Result<()> {
        let data = serialize_job(job)?;
        let next_trigger_us = job.next_trigger_at.map(to_us).unwrap_or(0);
        let id = job.id.clone();
        let user_id = job.user_id.clone();
        let status = job.status.as_str().to_string();
        let affected = self
            .pool
            .interact_write("cron.save", move |conn| {
                // `pinned` is deliberately NOT in this SET list — it is owned by
                // the targeted `set_pinned`, exactly like `sessions.pinned`. This
                // is a whole-blob last-writer-wins UPDATE from whatever snapshot
                // the caller holds, and `Scheduler::advance_recurring` calls it on
                // EVERY fire with the job it read back at `list_due` — so a
                // `pinned` written here would revert the user's pin on the job's
                // next tick, minutes after they set it.
                Ok(conn.execute(
                    "UPDATE cron_jobs SET user_id = ?1, status = ?2, next_trigger_at = ?3, \
                     data = ?4 WHERE id = ?5",
                    rusqlite::params![user_id, status, next_trigger_us, data, id],
                )?)
            })
            .await?;

        if affected == 0 {
            return Err(StorageError::NotFound(job.id.clone()));
        }
        Ok(())
    }

    async fn set_pinned(&self, job_id: &str, pinned: bool) -> Result<bool> {
        let id = job_id.to_string();
        let flag: i64 = if pinned { 1 } else { 0 };
        // Targeted UPDATE on the flat column only — the JSON `data` blob is left
        // alone, so a full-blob write (`save`, `save_if_unchanged`, `record_fire`)
        // cannot lose it. Reads patch `CronJob.pinned` from the column.
        let affected = self
            .pool
            .interact_write("cron.set_pinned", move |conn| {
                Ok(conn.execute(
                    "UPDATE cron_jobs SET pinned = ?2 WHERE id = ?1",
                    rusqlite::params![id, flag],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn save_if_unchanged(&self, job: &CronJob, expected: &CronJob) -> Result<bool> {
        let data = serialize_job(job)?;
        let next_trigger_us = job.next_trigger_at.map(to_us).unwrap_or(0);
        let id = job.id.clone();
        let user_id = job.user_id.clone();
        let status = job.status.as_str().to_string();
        let expected_updated_at = expected.updated_at;
        let expected = Unmoved::from(expected);

        self.pool
            .interact_write("cron.save_if_unchanged", move |conn| {
                // The read and the write are one transaction: this write replaces
                // the whole blob, so it may only land on the row the caller read
                // — every field of it, not just the two the columns project.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let stored: Option<String> = tx
                    .query_row(
                        &format!("SELECT data FROM cron_jobs WHERE id = :id AND {UNMOVED}"),
                        rusqlite::named_params! {
                            ":id": id,
                            ":expected_status": expected.status,
                            ":expected_next_trigger_at": expected.next_trigger_us,
                        },
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(stored) = stored else {
                    return Ok(false);
                };

                // A write that touches only the blob — an edit's new prompt, a
                // pause's status — moves neither column, so the columns alone
                // cannot see it. `updated_at` is the one field every writer
                // stamps, which makes it the row's version: a snapshot still
                // carrying the stored one is a snapshot nothing has landed on.
                if job_from_row(None, 0, 0, &stored)?.updated_at != expected_updated_at {
                    return Ok(false);
                }

                tx.execute(
                    "UPDATE cron_jobs SET user_id = ?1, status = ?2, next_trigger_at = ?3, \
                     data = ?4 WHERE id = ?5",
                    rusqlite::params![user_id, status, next_trigger_us, data, id],
                )?;
                tx.commit()?;
                Ok(true)
            })
            .await
    }

    async fn record_fire(&self, expected: &CronJob, fire: CronFire) -> Result<bool> {
        let id = expected.id.clone();
        let expected = Unmoved::from(expected);

        self.pool
            .interact_write("cron.record_fire", move |conn| {
                // The read and the write are one transaction, so the blob this
                // rewrites is the one the row carries *now*. An edit that landed
                // while the slot was firing lives in that blob, and a write-back
                // that re-serialized the pre-fire snapshot instead would revert
                // it.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let data: Option<String> = tx
                    .query_row(
                        &format!("SELECT data FROM cron_jobs WHERE id = :id AND {UNMOVED}"),
                        rusqlite::named_params! {
                            ":id": id,
                            ":expected_status": expected.status,
                            ":expected_next_trigger_at": expected.next_trigger_us,
                        },
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(data) = data else {
                    return Ok(false);
                };

                let mut job = job_from_row(None, 0, 0, &data)?;
                job.status = fire.status.clone();
                job.next_trigger_at = fire.next_trigger_at;
                job.last_triggered_at = Some(fire.last_triggered_at);
                job.updated_at = fire.updated_at;

                tx.execute(
                    "UPDATE cron_jobs SET status = ?1, next_trigger_at = ?2, data = ?3 \
                     WHERE id = ?4",
                    rusqlite::params![
                        job.status.as_str(),
                        job.next_trigger_at.map(to_us).unwrap_or(0),
                        serialize_job(&job)?,
                        id,
                    ],
                )?;
                tx.commit()?;
                Ok(true)
            })
            .await
    }

    async fn delete(&self, job_id: &str) -> Result<()> {
        let job = self.load_job(job_id).await?;
        // A built-in job is part of the runtime, switched off in config and
        // paused from the UI — never deleted, because nothing would recreate
        // the user's schedule for it. Enforced here so no caller can route
        // around the gateway's own refusal.
        if job.builtin {
            return Err(StorageError::Conflict(format!(
                "cron job {job_id} is built in and cannot be deleted; pause it instead"
            )));
        }
        if job.is_deleted() {
            return Ok(());
        }
        self.write_deleted_at(job_id, Some(Utc::now())).await
    }

    async fn restore(&self, job_id: &str) -> Result<()> {
        self.load_job(job_id).await?;
        self.write_deleted_at(job_id, None).await
    }

    async fn list_by_user(&self, user_id: &str) -> Result<Vec<CronJob>> {
        let user_id = user_id.to_string();
        let rows = self
            .pool
            .interact("cron.list_by_user", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {JOB_COLS} FROM cron_jobs WHERE user_id = ?1 AND {LIVE_ONLY}"
                ))?;
                let rows = stmt
                    .query_map(rusqlite::params![user_id], job_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        decode_jobs(rows)
    }

    async fn list_all(&self) -> Result<Vec<CronJob>> {
        let rows = self
            .pool
            .interact("cron.list_all", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {JOB_COLS} FROM cron_jobs WHERE {LIVE_ONLY}"
                ))?;
                let rows = stmt
                    .query_map([], job_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        decode_jobs(rows)
    }

    async fn list_enabled(&self) -> Result<Vec<CronJob>> {
        let rows = self
            .pool
            .interact("cron.list_enabled", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {JOB_COLS} FROM cron_jobs WHERE status = 'enabled' AND {LIVE_ONLY}"
                ))?;
                let rows = stmt
                    .query_map([], job_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        decode_jobs(rows)
    }

    async fn list_deleted(&self) -> Result<Vec<CronJob>> {
        let rows = self
            .pool
            .interact("cron.list_deleted", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {JOB_COLS} FROM cron_jobs \
                     WHERE deleted_at IS NOT NULL ORDER BY deleted_at DESC"
                ))?;
                let rows = stmt
                    .query_map([], job_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        decode_jobs(rows)
    }

    async fn list_due(&self, now_us: i64) -> Result<Vec<CronJob>> {
        let rows = self
            .pool
            .interact("cron.list_due", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {JOB_COLS} FROM cron_jobs \
                     WHERE {LIVE_ONLY} AND status = 'enabled' \
                     AND next_trigger_at != 0 AND next_trigger_at <= ?1"
                ))?;
                let rows = stmt
                    .query_map(rusqlite::params![now_us], job_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        decode_jobs(rows)
    }

    // ── Execution records ──

    async fn record_execution_if_job_unchanged(
        &self,
        exec: &CronExecution,
        expected_job: &CronJob,
    ) -> Result<bool> {
        let data = serialize_execution(exec)?;
        let scheduled_us = exec.scheduled_fire_time.timestamp_micros();
        let triggered_us = exec.triggered_at.timestamp_micros();
        let execution_id = exec.id.clone();
        let job_id = exec.job_id.clone();
        let user_id = exec.user_id.clone();
        let status = execution_status_str(exec.status).to_string();
        let expected_updated_at = expected_job.updated_at;
        let expected = Unmoved::from(expected_job);

        let outcome = self
            .pool
            .interact_write("cron.record_execution_if_job_unchanged", move |conn| {
                // Take the write lock before checking the row. An edit that
                // committed first makes this return false; one that commits after
                // this transaction necessarily happened after the execution row
                // (and its documented fire-time snapshot) already existed.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let stored: Option<String> = tx
                    .query_row(
                        &format!("SELECT data FROM cron_jobs WHERE id = :id AND {UNMOVED}"),
                        rusqlite::named_params! {
                            ":id": job_id,
                            ":expected_status": expected.status,
                            ":expected_next_trigger_at": expected.next_trigger_us,
                        },
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(stored) = stored else {
                    return Ok(0_u8);
                };
                let stored_job = job_from_row(None, 0, 0, &stored)?;
                if stored_job.updated_at != expected_updated_at {
                    return Ok(0_u8);
                }

                let affected = tx.execute(
                    "INSERT OR IGNORE INTO cron_executions (id, job_id, user_id, scheduled_fire_time, triggered_at, status, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        execution_id,
                        job_id,
                        user_id,
                        scheduled_us,
                        triggered_us,
                        status,
                        data,
                    ],
                )?;
                if affected == 0 {
                    return Ok(2_u8);
                }
                tx.commit()?;
                Ok(1_u8)
            })
            .await?;
        match outcome {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(StorageError::Conflict(format!(
                "{}@{}",
                exec.job_id, scheduled_us
            ))),
        }
    }

    async fn list_executions_by_job(&self, job_id: &str) -> Result<Vec<CronExecution>> {
        let job_id = job_id.to_string();
        let rows = self
            .pool
            .interact("cron.list_executions_by_job", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, job_id, user_id, scheduled_fire_time, triggered_at, status, data FROM cron_executions WHERE job_id = ?1 ORDER BY triggered_at DESC",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![job_id], |row| {
                        Ok((row.get::<_, String>(5)?, row.get::<_, String>(6)?))
                    })?
                    .collect::<rusqlite::Result<Vec<ExecutionRow>>>()?;
                Ok(rows)
            })
            .await?;
        decode_executions(rows)
    }

    async fn list_executions_by_user(&self, user_id: &str) -> Result<Vec<CronExecution>> {
        let user_id = user_id.to_string();
        let rows = self
            .pool
            .interact("cron.list_executions_by_user", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, job_id, user_id, scheduled_fire_time, triggered_at, status, data FROM cron_executions WHERE user_id = ?1 ORDER BY triggered_at DESC",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![user_id], |row| {
                        Ok((row.get::<_, String>(5)?, row.get::<_, String>(6)?))
                    })?
                    .collect::<rusqlite::Result<Vec<ExecutionRow>>>()?;
                Ok(rows)
            })
            .await?;
        decode_executions(rows)
    }

    async fn has_execution_for_schedule(
        &self,
        job_id: &str,
        scheduled_fire_time_us: i64,
    ) -> Result<bool> {
        let job_id = job_id.to_string();
        self.pool
            .interact("cron.has_execution_for_schedule", move |conn| {
                let found: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM cron_executions WHERE job_id = ?1 AND scheduled_fire_time = ?2 LIMIT 1",
                        rusqlite::params![job_id, scheduled_fire_time_us],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;
                Ok(found.is_some())
            })
            .await
    }

    async fn update_execution_status(
        &self,
        execution_id: &str,
        status: ExecutionStatus,
    ) -> Result<()> {
        let status = execution_status_str(status).to_string();
        let id = execution_id.to_string();
        let affected = self
            .pool
            .interact_write("cron.update_execution_status", move |conn| {
                Ok(conn.execute(
                    "UPDATE cron_executions SET status = ?1 WHERE id = ?2",
                    rusqlite::params![status, id],
                )?)
            })
            .await?;

        if affected == 0 {
            return Err(StorageError::NotFound(execution_id.to_string()));
        }
        Ok(())
    }

    async fn list_executions_by_status(
        &self,
        status: ExecutionStatus,
    ) -> Result<Vec<CronExecution>> {
        let status = execution_status_str(status).to_string();
        let rows = self
            .pool
            .interact("cron.list_executions_by_status", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {EXECUTION_COLS} FROM cron_executions WHERE status = ?1"
                ))?;
                let rows = stmt
                    .query_map(rusqlite::params![status], |row| {
                        Ok((row.get::<_, String>(5)?, row.get::<_, String>(6)?))
                    })?
                    .collect::<rusqlite::Result<Vec<ExecutionRow>>>()?;
                Ok(rows)
            })
            .await?;
        decode_executions(rows)
    }

    // ── Delivery ledger ──
    //
    // The full ledger lives in the `data` blob (read-modify-write below);
    // `completed_at` / `notified_at` are also projected into columns so the
    // boot re-drive's scan is an indexed query rather than a full-table
    // deserialize. Both writers are single (the waiter stamps completion, the
    // origin actor stamps the resolution), so no read-modify-write race.

    async fn record_execution_completion(
        &self,
        execution_id: &str,
        completion: ExecutionCompletion,
    ) -> Result<()> {
        let mut exec = self.load_execution(execution_id).await?;
        exec.fire_session_id = Some(completion.fire_session_id);
        exec.outcome = Some(completion.outcome);
        exec.reply_ordinal = completion.reply_ordinal;
        exec.completed_at = Some(completion.completed_at);
        let data = serialize_execution(&exec)?;
        let completed_us = completion.completed_at.timestamp_micros();
        let id = execution_id.to_string();

        self.pool
            .interact_write("cron.record_execution_completion", move |conn| {
                conn.execute(
                    "UPDATE cron_executions SET completed_at = ?1, data = ?2 WHERE id = ?3",
                    rusqlite::params![completed_us, data, id],
                )?;
                Ok(())
            })
            .await
    }

    async fn mark_execution_notified(&self, execution_id: &str, at: DateTime<Utc>) -> Result<()> {
        let mut exec = self.load_execution(execution_id).await?;
        exec.notified_at = Some(at);
        let data = serialize_execution(&exec)?;
        let at_us = at.timestamp_micros();
        let id = execution_id.to_string();

        self.pool
            .interact_write("cron.mark_execution_notified", move |conn| {
                conn.execute(
                    "UPDATE cron_executions SET notified_at = ?1, data = ?2 WHERE id = ?3",
                    rusqlite::params![at_us, data, id],
                )?;
                Ok(())
            })
            .await
    }

    async fn list_executions_awaiting_delivery(&self) -> Result<Vec<CronExecution>> {
        let rows = self
            .pool
            .interact("cron.list_executions_awaiting_delivery", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {EXECUTION_COLS} FROM cron_executions \
                     WHERE completed_at IS NOT NULL AND notified_at IS NULL \
                     ORDER BY completed_at ASC"
                ))?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(5)?, row.get::<_, String>(6)?))
                    })?
                    .collect::<rusqlite::Result<Vec<ExecutionRow>>>()?;
                Ok(rows)
            })
            .await?;
        decode_executions(rows)
    }
}

impl SqliteCronStore {
    async fn load_job(&self, job_id: &str) -> Result<CronJob> {
        self.get(job_id)
            .await?
            .ok_or_else(|| StorageError::NotFound(job_id.to_string()))
    }

    /// Move a job into or out of the recycle bin. Touches the `deleted_at`
    /// column and nothing else: the row's `data` blob is owned by `save`, and
    /// re-writing it from a snapshot loaded here would revert a fire's
    /// write-back that landed in between.
    async fn write_deleted_at(
        &self,
        job_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let deleted_us = deleted_at.map(to_us);
        let id = job_id.to_string();
        let affected = self
            .pool
            .interact_write("cron.write_deleted_at", move |conn| {
                Ok(conn.execute(
                    "UPDATE cron_jobs SET deleted_at = ?1 WHERE id = ?2",
                    rusqlite::params![deleted_us, id],
                )?)
            })
            .await?;

        if affected == 0 {
            return Err(StorageError::NotFound(job_id.to_string()));
        }
        Ok(())
    }

    /// Load one execution for a read-modify-write of its ledger. The row's
    /// `data` blob carries every ledger field; the `status` column overrides
    /// the blob's copy (see [`execution_from_row`]).
    async fn load_execution(&self, execution_id: &str) -> Result<CronExecution> {
        let id = execution_id.to_string();
        let row: Option<ExecutionRow> = self
            .pool
            .interact("cron.load_execution", move |conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT {EXECUTION_COLS} FROM cron_executions WHERE id = ?1"),
                        rusqlite::params![id],
                        |row| Ok((row.get::<_, String>(5)?, row.get::<_, String>(6)?)),
                    )
                    .optional()?)
            })
            .await?;

        match row {
            Some((status, data)) => execution_from_row(&status, &data),
            None => Err(StorageError::NotFound(execution_id.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::ChannelType;
    use baybo_model::{CronSchedule, CronStatus};
    use chrono::Utc;
    fn future_dt() -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn past_dt() -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// A "now" between [`past_dt`] and [`future_dt`], as `list_due` takes it.
    fn now_us() -> i64 {
        chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            .timestamp_micros()
    }

    fn test_job(id: &str, user_id: &str, status: CronStatus) -> CronJob {
        let now = Utc::now();
        CronJob {
            id: id.to_string(),
            user_id: user_id.to_string(),
            channel: ChannelType::tui(),
            title: "test job".to_string(),
            schedule: CronSchedule::cron("0 9 * * *"),
            prompt: "test".to_string(),
            timezone: "UTC".to_string(),
            status,
            last_triggered_at: None,
            next_trigger_at: Some(future_dt()),
            created_at: now,
            updated_at: now,
            project_id: None,
            origin_session_id: None,
            deleted_at: None,
            pinned: false,
            builtin: false,
        }
    }

    fn test_execution(id: &str, job_id: &str, user_id: &str) -> CronExecution {
        // Salt the schedule slot with the id's hash so the
        // UNIQUE(job_id, scheduled_fire_time) constraint isn't tripped
        // by repeat fixture rows.
        let salt: i64 = id.chars().map(|c| c as i64).sum();
        let scheduled = future_dt() + chrono::Duration::microseconds(salt);
        let mut exec = CronExecution::pending(
            &test_job(job_id, user_id, CronStatus::Enabled),
            scheduled,
            future_dt(),
        );
        exec.id = id.to_string();
        exec
    }

    async fn record_test_execution(store: &SqliteCronStore, execution: CronExecution) {
        let job = match store.get(&execution.job_id).await.unwrap() {
            Some(job) => job,
            None => {
                let job = test_job(&execution.job_id, &execution.user_id, CronStatus::Enabled);
                store.create(&job).await.unwrap();
                job
            }
        };
        assert!(
            store
                .record_execution_if_job_unchanged(&execution, &job)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn create_and_get() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        store
            .create(&test_job("cj-1", "u1", CronStatus::Enabled))
            .await
            .unwrap();
        let loaded = store.get("cj-1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "cj-1");
        assert_eq!(loaded.user_id, "u1");
    }

    /// The whole reason `pinned` is a flat column and not a blob field. Every
    /// blob write reconstructs the row from a snapshot the caller holds, and
    /// `record_fire` does it on EVERY fire from the pre-fire job — a snapshot
    /// taken *before* the user pinned. If a fire (or a plain `save`) wrote
    /// `pinned`, the pin would be reverted by the job's next tick.
    #[tokio::test]
    async fn a_fire_and_a_save_cannot_unpin_the_group() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        store
            .create(&test_job("cj-1", "u1", CronStatus::Enabled))
            .await
            .unwrap();

        // The scheduler reads the job (unpinned) …
        let stale = store.get("cj-1").await.unwrap().unwrap();
        assert!(!stale.pinned);

        // … the user pins the group …
        assert!(store.set_pinned("cj-1", true).await.unwrap());
        assert!(store.get("cj-1").await.unwrap().unwrap().pinned);

        // … and the job fires, writing back from the pre-pin snapshot. The fire
        // is conditional on the row being unmoved, and it is — `set_pinned`
        // touched neither `status` nor `next_trigger_at` — so it lands, and must
        // NOT carry the snapshot's stale `pinned = false` back.
        assert!(
            store
                .record_fire(&stale, advance_to(future_dt()))
                .await
                .unwrap()
        );
        assert!(
            store.get("cj-1").await.unwrap().unwrap().pinned,
            "the fire clobbered the pin — `pinned` leaked into `record_fire`/the data blob"
        );

        // A plain `save` (a create/edit's whole-row write) is just as blind to
        // the column: a stale copy that thinks it is unpinned must not unpin it.
        let mut edited = store.get("cj-1").await.unwrap().unwrap();
        edited.pinned = false;
        edited.prompt = "edited".to_string();
        store.save(&edited).await.unwrap();
        assert!(
            store.get("cj-1").await.unwrap().unwrap().pinned,
            "a whole-row `save` clobbered the pin"
        );
    }

    #[tokio::test]
    async fn set_pinned_round_trips_and_reports_a_missing_job() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        store
            .create(&test_job("cj-1", "u1", CronStatus::Enabled))
            .await
            .unwrap();

        assert!(store.set_pinned("cj-1", true).await.unwrap());
        assert!(store.list_all().await.unwrap()[0].pinned);
        assert!(store.set_pinned("cj-1", false).await.unwrap());
        assert!(!store.list_all().await.unwrap()[0].pinned);
        // A job that no longer exists (deleted from another client) reports it
        // rather than erroring — the route answers 404 off this.
        assert!(!store.set_pinned("gone", true).await.unwrap());
    }

    /// The pin must never reach the JSON blob — that is what makes `save` safe.
    #[tokio::test]
    async fn the_pin_is_not_in_the_data_blob() {
        let job = test_job("cj-1", "u1", CronStatus::Enabled);
        let blob = serialize_job(&job).unwrap();
        assert!(
            !blob.contains("pinned"),
            "`CronJob::pinned` lost its #[serde(skip)] — it is now clobberable by `save`: {blob}"
        );
    }

    #[tokio::test]
    async fn save_updates_row() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("cj-2", "u1", CronStatus::Enabled);
        store.create(&job).await.unwrap();

        job.status = CronStatus::Disabled;
        store.save(&job).await.unwrap();

        let loaded = store.get("cj-2").await.unwrap().unwrap();
        assert_eq!(loaded.status, CronStatus::Disabled);
    }

    #[tokio::test]
    async fn save_nonexistent_returns_not_found() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let err = store
            .save(&test_job("nonexistent", "u1", CronStatus::Enabled))
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    /// The recycle bin: a deleted job leaves every listing but stays
    /// resolvable by id, keeping its status untouched.
    #[tokio::test]
    async fn delete_hides_the_job_from_every_listing_but_keeps_the_row() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("cj-3", "u1", CronStatus::Enabled);
        job.next_trigger_at = Some(past_dt());
        store.create(&job).await.unwrap();

        store.delete("cj-3").await.unwrap();

        let deleted = store
            .get("cj-3")
            .await
            .unwrap()
            .expect("a deleted job still resolves by id");
        assert!(deleted.is_deleted());
        assert_eq!(
            deleted.status,
            CronStatus::Enabled,
            "deletion is orthogonal to status"
        );

        assert!(store.list_by_user("u1").await.unwrap().is_empty());
        assert!(store.list_all().await.unwrap().is_empty());
        assert!(store.list_enabled().await.unwrap().is_empty());
        assert!(
            store.list_due(now_us()).await.unwrap().is_empty(),
            "a deleted job must never reach the tick loop"
        );

        let bin = store.list_deleted().await.unwrap();
        assert_eq!(bin.len(), 1);
        assert_eq!(bin[0].id, "cj-3");
    }

    #[tokio::test]
    async fn restore_brings_the_job_back_into_the_listings() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        store
            .create(&test_job("cj-restore", "u1", CronStatus::Enabled))
            .await
            .unwrap();

        store.delete("cj-restore").await.unwrap();
        store.restore("cj-restore").await.unwrap();

        let restored = store.get("cj-restore").await.unwrap().unwrap();
        assert!(!restored.is_deleted());
        assert_eq!(store.list_by_user("u1").await.unwrap().len(), 1);
        assert_eq!(store.list_enabled().await.unwrap().len(), 1);
        assert!(store.list_deleted().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_builtin_job_survives_delete_but_is_otherwise_ordinary() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("baybo-dream", "owner", CronStatus::Enabled);
        job.builtin = true;
        store.create(&job).await.unwrap();

        // The flat column round-trips even though the blob never carries it.
        let loaded = store.get("baybo-dream").await.unwrap().expect("job");
        assert!(loaded.builtin);

        let err = store.delete("baybo-dream").await.unwrap_err();
        assert!(matches!(err, StorageError::Conflict(_)), "got: {err:?}");
        let still_live = store.get("baybo-dream").await.unwrap().expect("job");
        assert!(!still_live.is_deleted(), "a built-in job is never binned");

        // Pausing it, on the other hand, is exactly how it is switched off.
        let mut paused = still_live.clone();
        paused.status = CronStatus::Disabled;
        store.save(&paused).await.unwrap();
        let after = store.get("baybo-dream").await.unwrap().expect("job");
        assert_eq!(after.status, CronStatus::Disabled);
        assert!(after.builtin, "a save must not clear the flat column");
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let err = store.delete("nonexistent").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn restore_nonexistent_returns_not_found() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let err = store.restore("nonexistent").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    /// The write-back of a fire that advanced a recurring job to its next slot.
    fn advance_to(next: DateTime<Utc>) -> CronFire {
        CronFire {
            status: CronStatus::Enabled,
            next_trigger_at: Some(next),
            last_triggered_at: past_dt(),
            updated_at: past_dt(),
        }
    }

    /// The tick loop's post-fire write-back must not undo a pause that landed
    /// while the slot was firing — the row would be re-armed from a stale
    /// snapshot and keep firing, with the user's stop control silently lost.
    #[tokio::test]
    async fn the_post_fire_write_back_is_dropped_when_the_job_was_paused_or_deleted() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("cj-pause", "u1", CronStatus::Enabled);
        job.next_trigger_at = Some(past_dt());
        store.create(&job).await.unwrap();

        let in_flight = store.get("cj-pause").await.unwrap().unwrap();

        let mut paused = in_flight.clone();
        paused.status = CronStatus::Disabled;
        paused.next_trigger_at = None;
        store.save(&paused).await.unwrap();

        assert!(
            !store
                .record_fire(&in_flight, advance_to(future_dt()))
                .await
                .unwrap(),
            "the write-back landed on a paused row",
        );

        let after = store.get("cj-pause").await.unwrap().unwrap();
        assert_eq!(after.status, CronStatus::Disabled);
        assert!(after.next_trigger_at.is_none());
        assert!(after.last_triggered_at.is_none());
        assert!(store.list_due(now_us()).await.unwrap().is_empty());

        store.restore("cj-pause").await.unwrap();
        store.save(&in_flight).await.unwrap();
        store.delete("cj-pause").await.unwrap();
        assert!(
            !store
                .record_fire(&in_flight, advance_to(future_dt()))
                .await
                .unwrap(),
            "the write-back landed on a deleted row",
        );
    }

    /// A fire owns when it ran and where the schedule goes next. It does not own
    /// the prompt — so an edit that landed while the slot was firing survives the
    /// write-back, which reads the row as it now stands rather than re-serializing
    /// the snapshot it was handed before the fire.
    #[tokio::test]
    async fn a_fires_write_back_keeps_an_edit_that_landed_mid_fire() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("cj-edit", "u1", CronStatus::Enabled);
        job.next_trigger_at = Some(past_dt());
        job.prompt = "old prompt".to_string();
        store.create(&job).await.unwrap();

        // The snapshot the tick loop read before firing the slot.
        let in_flight = store.get("cj-edit").await.unwrap().unwrap();

        // The user edits the prompt while the slot is firing.
        let mut edited = in_flight.clone();
        edited.prompt = "new prompt".to_string();
        edited.title = "new title".to_string();
        assert!(store.save_if_unchanged(&edited, &in_flight).await.unwrap());

        // The fire writes back, carrying the snapshot from before the edit.
        assert!(
            store
                .record_fire(&in_flight, advance_to(future_dt()))
                .await
                .unwrap(),
            "the write-back was dropped; the fire went unrecorded",
        );

        let after = store.get("cj-edit").await.unwrap().unwrap();
        assert_eq!(
            after.prompt, "new prompt",
            "the fire's write-back reverted the user's edit",
        );
        assert_eq!(after.title, "new title");
        assert_eq!(after.next_trigger_at, Some(future_dt()));
        assert_eq!(after.last_triggered_at, Some(past_dt()));
    }

    /// When the edit that landed mid-fire *rescheduled* the job, the write-back
    /// has nothing to say: advancing the schedule the user just replaced would
    /// overwrite the fire time they chose. The row moved, so the write is dropped.
    #[tokio::test]
    async fn a_fires_write_back_is_dropped_when_the_job_was_rescheduled_mid_fire() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("cj-resched", "u1", CronStatus::Enabled);
        job.next_trigger_at = Some(past_dt());
        store.create(&job).await.unwrap();

        let in_flight = store.get("cj-resched").await.unwrap().unwrap();

        let user_slot = future_dt() + chrono::Duration::days(1);
        let mut rescheduled = in_flight.clone();
        rescheduled.next_trigger_at = Some(user_slot);
        assert!(
            store
                .save_if_unchanged(&rescheduled, &in_flight)
                .await
                .unwrap()
        );

        assert!(
            !store
                .record_fire(&in_flight, advance_to(future_dt()))
                .await
                .unwrap(),
            "the write-back advanced a schedule the user had already replaced",
        );

        let after = store.get("cj-resched").await.unwrap().unwrap();
        assert_eq!(after.next_trigger_at, Some(user_slot));
    }

    /// A conditional save is refused once the row has moved — the fire that
    /// landed in the edit's window would otherwise be undone: its slot re-armed,
    /// its `last_triggered_at` forgotten.
    #[tokio::test]
    async fn a_save_is_refused_once_the_row_has_moved_or_gone() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("cj-cas", "u1", CronStatus::Enabled);
        job.next_trigger_at = Some(past_dt());
        store.create(&job).await.unwrap();

        // What an in-flight edit read, before the slot fired.
        let read_before_the_fire = store.get("cj-cas").await.unwrap().unwrap();
        assert!(
            store
                .record_fire(&read_before_the_fire, advance_to(future_dt()))
                .await
                .unwrap()
        );

        let mut stale = read_before_the_fire.clone();
        stale.prompt = "edited".to_string();
        assert!(
            !store
                .save_if_unchanged(&stale, &read_before_the_fire)
                .await
                .unwrap(),
            "a write carrying a pre-fire snapshot landed on the fired row",
        );

        let after = store.get("cj-cas").await.unwrap().unwrap();
        assert_eq!(after.prompt, "test", "the stale write landed anyway");
        assert_eq!(after.next_trigger_at, Some(future_dt()));
        assert_eq!(after.last_triggered_at, Some(past_dt()));

        // Re-applied to the row as it now stands, the same edit lands.
        let mut fresh = after.clone();
        fresh.prompt = "edited".to_string();
        assert!(store.save_if_unchanged(&fresh, &after).await.unwrap());
        assert_eq!(store.get("cj-cas").await.unwrap().unwrap().prompt, "edited");

        // And a row in the recycle bin takes no edit at all.
        store.delete("cj-cas").await.unwrap();
        let live = store.get("cj-cas").await.unwrap().unwrap();
        let mut binned_edit = live.clone();
        binned_edit.prompt = "edited again".to_string();
        assert!(
            !store.save_if_unchanged(&binned_edit, &live).await.unwrap(),
            "a binned job was edited",
        );
    }

    /// Two writes that move neither column — a prompt edit and a title edit, or
    /// a pause landing on a snapshot read before an edit — are invisible to the
    /// `status` / `next_trigger_at` pair. The row's `updated_at` is what sees
    /// them: the second write is refused rather than silently reverting the
    /// first, and the caller re-applies it to the row as it now stands.
    #[tokio::test]
    async fn a_write_that_moved_only_the_blob_is_still_seen_by_the_next_one() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("cj-blob-cas", "u1", CronStatus::Enabled);
        job.next_trigger_at = Some(future_dt());
        job.prompt = "old prompt".to_string();
        store.create(&job).await.unwrap();

        // Both editors read the same snapshot.
        let snapshot = store.get("cj-blob-cas").await.unwrap().unwrap();

        let mut prompt_edit = snapshot.clone();
        prompt_edit.prompt = "new prompt".to_string();
        prompt_edit.updated_at = snapshot.updated_at + chrono::Duration::seconds(1);
        assert!(
            store
                .save_if_unchanged(&prompt_edit, &snapshot)
                .await
                .unwrap()
        );

        // The second editor's row moved under it — same status, same slot.
        let mut title_edit = snapshot.clone();
        title_edit.title = "Evening digest".to_string();
        title_edit.updated_at = snapshot.updated_at + chrono::Duration::seconds(2);
        assert!(
            !store
                .save_if_unchanged(&title_edit, &snapshot)
                .await
                .unwrap(),
            "a stale blob landed and reverted the edit before it",
        );
        assert_eq!(
            store.get("cj-blob-cas").await.unwrap().unwrap().prompt,
            "new prompt",
        );
    }

    /// A job with no slot — paused, or a one-shot that has run — is still a job
    /// a user edits, and its edit goes through the same conditional write. The
    /// snapshot it compares against holds no `next_trigger_at`, so this is the
    /// one path where [`UNMOVED`] matches the column against the *absence* of a
    /// fire time. It has to match: were an empty slot ever stored as SQL `NULL`,
    /// `next_trigger_at = :expected` would be true of nothing, and every edit of
    /// a paused job would lose a race it was never in.
    #[tokio::test]
    async fn a_job_holding_no_slot_is_still_editable() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);

        for (id, status) in [
            ("cj-paused", CronStatus::Disabled),
            ("cj-fired", CronStatus::Executed),
        ] {
            let mut job = test_job(id, "u1", status.clone());
            job.next_trigger_at = None;
            store.create(&job).await.unwrap();

            let current = store.get(id).await.unwrap().unwrap();
            assert!(current.next_trigger_at.is_none());

            let mut edited = current.clone();
            edited.prompt = "edited".to_string();
            assert!(
                store.save_if_unchanged(&edited, &current).await.unwrap(),
                "a {} job could not be edited: its empty slot never matched",
                status.as_str(),
            );

            let after = store.get(id).await.unwrap().unwrap();
            assert_eq!(after.prompt, "edited");
            assert_eq!(after.status, status);
            assert!(after.next_trigger_at.is_none(), "the edit armed it");
        }
    }

    /// Re-arming a one-shot that has already run: the edit gives it a status and
    /// a slot it did not have, and the row it lands on is matched on the empty
    /// slot the fire left behind.
    #[tokio::test]
    async fn a_fired_one_shot_is_re_armed_by_an_edit() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("cj-once", "u1", CronStatus::Enabled);
        job.schedule = CronSchedule::at(past_dt());
        job.next_trigger_at = Some(past_dt());
        store.create(&job).await.unwrap();

        let in_flight = store.get("cj-once").await.unwrap().unwrap();
        assert!(
            store
                .record_fire(
                    &in_flight,
                    CronFire {
                        status: CronStatus::Executed,
                        next_trigger_at: None,
                        last_triggered_at: past_dt(),
                        updated_at: past_dt(),
                    },
                )
                .await
                .unwrap()
        );

        let fired = store.get("cj-once").await.unwrap().unwrap();
        assert_eq!(fired.status, CronStatus::Executed);
        assert!(fired.next_trigger_at.is_none());

        let mut re_armed = fired.clone();
        re_armed.schedule = CronSchedule::at(future_dt());
        re_armed.next_trigger_at = Some(future_dt());
        re_armed.status = CronStatus::Enabled;
        assert!(
            store.save_if_unchanged(&re_armed, &fired).await.unwrap(),
            "a fired one-shot could not be given a new moment",
        );

        let after = store.get("cj-once").await.unwrap().unwrap();
        assert_eq!(after.status, CronStatus::Enabled);
        assert_eq!(after.next_trigger_at, Some(future_dt()));
        assert_eq!(
            after.last_triggered_at,
            Some(past_dt()),
            "re-arming the job threw away the run it had already done",
        );
        // And it is on the schedule again — the `enabled` + slot pair the tick
        // loop selects on, not just a blob that says so.
        assert!(
            store
                .list_due(future_dt().timestamp_micros() + 1)
                .await
                .unwrap()
                .iter()
                .any(|j| j.id == "cj-once"),
        );
    }

    /// `delete` / `restore` own the `deleted_at` column and touch nothing else:
    /// a fire's write-back that lands between the load and the stamp must not be
    /// reverted by a blob re-serialized from the pre-fire snapshot.
    #[tokio::test]
    async fn deleting_a_job_does_not_revert_a_fires_write_back() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let job = test_job("cj-blob", "u1", CronStatus::Enabled);
        store.create(&job).await.unwrap();

        let mut fired = store.get("cj-blob").await.unwrap().unwrap();
        fired.status = CronStatus::Executed;
        fired.last_triggered_at = Some(past_dt());
        store.save(&fired).await.unwrap();

        store.delete("cj-blob").await.unwrap();

        let after = store.get("cj-blob").await.unwrap().unwrap();
        assert!(after.is_deleted());
        assert_eq!(
            after.status,
            CronStatus::Executed,
            "the delete reverted the fire's write-back",
        );
        assert_eq!(after.last_triggered_at, Some(past_dt()));
    }

    /// A `save` racing a `delete` must not undo it. The tick loop reads a live
    /// job, works, then writes it back; if the user deletes the job inside that
    /// window, the write-back is carrying a snapshot that predates the deletion.
    /// The recycle-bin stamp belongs to the row, not to whatever snapshot a
    /// caller happens to hold, so the delete has to win — otherwise a job the
    /// user deleted comes back to life and keeps firing.
    #[tokio::test]
    async fn a_save_carrying_a_stale_live_snapshot_does_not_resurrect_a_deleted_job() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let mut job = test_job("cj-race", "u1", CronStatus::Enabled);
        job.next_trigger_at = Some(past_dt());
        store.create(&job).await.unwrap();

        // The tick loop's snapshot: read while the job was still live.
        let mut in_flight = store.get("cj-race").await.unwrap().unwrap();
        assert!(!in_flight.is_deleted());

        // The user deletes it mid-tick.
        store.delete("cj-race").await.unwrap();

        // The tick loop writes its snapshot back (as `advance_recurring` does).
        in_flight.next_trigger_at = Some(past_dt());
        store.save(&in_flight).await.unwrap();

        let after = store.get("cj-race").await.unwrap().expect("row survives");
        assert!(
            after.is_deleted(),
            "a stale write-back resurrected a deleted job",
        );
        assert!(
            store.list_due(now_us()).await.unwrap().is_empty(),
            "a resurrected job is firing again",
        );
        assert!(store.list_all().await.unwrap().is_empty());
        assert_eq!(store.list_deleted().await.unwrap().len(), 1);
    }

    /// Deleting twice is idempotent and keeps the *original* deletion time: the
    /// bin is ordered by it, and "deleted 3 days ago" must not become "deleted
    /// just now" because a retry or a double-click sent the delete again.
    #[tokio::test]
    async fn deleting_an_already_deleted_job_keeps_the_first_deletion_time() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        store
            .create(&test_job("cj-twice", "u1", CronStatus::Enabled))
            .await
            .unwrap();

        store.delete("cj-twice").await.unwrap();
        let first = store
            .get("cj-twice")
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .expect("the first delete stamps the row");

        store
            .delete("cj-twice")
            .await
            .expect("deleting a job already in the bin is a no-op, not an error");

        let after = store.get("cj-twice").await.unwrap().unwrap();
        assert_eq!(
            after.deleted_at,
            Some(first),
            "the second delete restamped the row and moved it in the bin",
        );
        assert_eq!(store.list_deleted().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_deleted_orders_most_recently_deleted_first() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        for id in ["cj-old", "cj-mid", "cj-new"] {
            store
                .create(&test_job(id, "u1", CronStatus::Enabled))
                .await
                .unwrap();
            store.delete(id).await.unwrap();
        }

        let bin: Vec<String> = store
            .list_deleted()
            .await
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(bin, vec!["cj-new", "cj-mid", "cj-old"]);
    }

    #[tokio::test]
    async fn list_by_user_filters_correctly() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        store
            .create(&test_job("cj-4", "u1", CronStatus::Enabled))
            .await
            .unwrap();
        store
            .create(&test_job("cj-5", "u2", CronStatus::Enabled))
            .await
            .unwrap();
        store
            .create(&test_job("cj-6", "u1", CronStatus::Disabled))
            .await
            .unwrap();

        let u1 = store.list_by_user("u1").await.unwrap();
        assert_eq!(u1.len(), 2);
    }

    #[tokio::test]
    async fn list_enabled_filters_disabled() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        store
            .create(&test_job("cj-7", "u1", CronStatus::Enabled))
            .await
            .unwrap();
        store
            .create(&test_job("cj-8", "u1", CronStatus::Disabled))
            .await
            .unwrap();

        let enabled = store.list_enabled().await.unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, "cj-7");
    }

    #[tokio::test]
    async fn list_due_returns_only_past_due() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);

        let mut past_due = test_job("cj-9", "u1", CronStatus::Enabled);
        past_due.next_trigger_at = Some(past_dt());
        store.create(&past_due).await.unwrap();

        let future = test_job("cj-10", "u1", CronStatus::Enabled);
        store.create(&future).await.unwrap();

        let mut disabled = test_job("cj-11", "u1", CronStatus::Disabled);
        disabled.next_trigger_at = Some(past_dt());
        store.create(&disabled).await.unwrap();

        // Enabled and long overdue, but in the recycle bin.
        let mut deleted = test_job("cj-12", "u1", CronStatus::Enabled);
        deleted.next_trigger_at = Some(past_dt());
        deleted.deleted_at = Some(past_dt());
        store.create(&deleted).await.unwrap();

        let due = store.list_due(now_us()).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "cj-9");
    }

    // ── Execution record tests ──

    #[tokio::test]
    async fn conditional_execution_insert_rejects_a_previously_committed_edit() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let expected = test_job("cj-edited", "u1", CronStatus::Enabled);
        store.create(&expected).await.unwrap();
        let stale_execution =
            CronExecution::pending(&expected, expected.next_trigger_at.unwrap(), Utc::now());

        let mut edited = expected.clone();
        edited.prompt = "the prompt the user actually wants now".to_string();
        edited.updated_at = expected.updated_at + chrono::Duration::seconds(1);
        assert!(store.save_if_unchanged(&edited, &expected).await.unwrap());

        assert!(
            !store
                .record_execution_if_job_unchanged(&stale_execution, &expected)
                .await
                .unwrap(),
            "a stale execution snapshot was inserted after an edit landed"
        );
        assert!(
            store
                .list_executions_by_job(&expected.id)
                .await
                .unwrap()
                .is_empty()
        );

        let current = store.get(&expected.id).await.unwrap().unwrap();
        let current_execution =
            CronExecution::pending(&current, current.next_trigger_at.unwrap(), Utc::now());
        assert!(
            store
                .record_execution_if_job_unchanged(&current_execution, &current)
                .await
                .unwrap()
        );
        assert_eq!(
            store.list_executions_by_job(&expected.id).await.unwrap()[0].prompt,
            "the prompt the user actually wants now",
        );
    }

    #[tokio::test]
    async fn record_execution_dedup_returns_already_dispatched() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);

        let job = test_job("cj-dup", "u1", CronStatus::Enabled);
        store.create(&job).await.unwrap();
        let mut exec = test_execution("ce-dup-a", "cj-dup", "u1");
        assert!(
            store
                .record_execution_if_job_unchanged(&exec, &job)
                .await
                .unwrap()
        );

        // Second instance of the same scheduler racing onto the same
        // (job_id, scheduled_fire_time) slot — different `id`, same dedup key.
        exec.id = "ce-dup-b".into();
        let err = store
            .record_execution_if_job_unchanged(&exec, &job)
            .await
            .unwrap_err();
        match err {
            StorageError::Conflict(key) => {
                assert!(key.starts_with("cj-dup@"), "key was {key}");
            }
            other => panic!("expected AlreadyDispatched, got {other:?}"),
        }

        // Original row remains; the loser produced no orphan.
        let execs = store.list_executions_by_job("cj-dup").await.unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].id, "ce-dup-a");
    }

    #[tokio::test]
    async fn record_and_list_executions_by_job() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);

        record_test_execution(&store, test_execution("ce-1", "cj-1", "u1")).await;
        record_test_execution(&store, test_execution("ce-2", "cj-1", "u1")).await;
        record_test_execution(&store, test_execution("ce-3", "cj-2", "u1")).await;

        let execs = store.list_executions_by_job("cj-1").await.unwrap();
        assert_eq!(execs.len(), 2);
    }

    #[tokio::test]
    async fn list_executions_by_user() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);

        record_test_execution(&store, test_execution("ce-4", "cj-1", "u1")).await;
        record_test_execution(&store, test_execution("ce-5", "cj-2", "u2")).await;
        record_test_execution(&store, test_execution("ce-6", "cj-3", "u1")).await;

        let u1 = store.list_executions_by_user("u1").await.unwrap();
        assert_eq!(u1.len(), 2);
    }

    #[tokio::test]
    async fn delivery_ledger_round_trips_and_drives_the_awaiting_scan() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);

        record_test_execution(&store, test_execution("ce-led", "cj-led", "u1")).await;
        // A dispatched-but-unfinished fire is not awaiting delivery yet.
        assert!(
            store
                .list_executions_awaiting_delivery()
                .await
                .unwrap()
                .is_empty()
        );

        let completed_at = future_dt();
        store
            .record_execution_completion(
                "ce-led",
                ExecutionCompletion {
                    fire_session_id: "cron-fire".into(),
                    outcome: baybo_model::ExecutionOutcome::Success,
                    reply_ordinal: Some(12),
                    completed_at,
                },
            )
            .await
            .unwrap();

        let awaiting = store.list_executions_awaiting_delivery().await.unwrap();
        assert_eq!(awaiting.len(), 1);
        let exec = &awaiting[0];
        assert_eq!(exec.id, "ce-led");
        assert_eq!(exec.reply_ordinal, Some(12));
        assert_eq!(
            exec.outcome,
            Some(baybo_model::ExecutionOutcome::Success),
            "the full ledger round-trips through the data blob"
        );
        assert_eq!(
            exec.fire_session_id.as_ref().map(|s| s.as_str()),
            Some("cron-fire")
        );
        assert_eq!(exec.completed_at, Some(completed_at));

        // Resolving the delivery takes it out of the scan for good.
        store
            .mark_execution_notified("ce-led", future_dt())
            .await
            .unwrap();
        assert!(
            store
                .list_executions_awaiting_delivery()
                .await
                .unwrap()
                .is_empty()
        );
        let stored = store.load_execution("ce-led").await.unwrap();
        assert!(stored.notified_at.is_some());
        assert!(!stored.awaits_delivery());
    }
    #[tokio::test]
    async fn ledger_writes_reject_unknown_execution() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);
        let err = store
            .mark_execution_notified("ghost", future_dt())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    /// A deleted job's execution records keep pointing at a job that still
    /// resolves — the provenance a fire's conversation is named from.
    #[tokio::test]
    async fn execution_records_of_a_deleted_job_still_name_a_real_job() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteCronStore::new(pool);

        store
            .create(&test_job("cj-evict", "u1", CronStatus::Enabled))
            .await
            .unwrap();
        record_test_execution(&store, test_execution("ce-7", "cj-evict", "u1")).await;

        store.delete("cj-evict").await.unwrap();

        let job = store.get("cj-evict").await.unwrap().expect("row survives");
        assert!(job.is_deleted());

        let execs = store.list_executions_by_job("cj-evict").await.unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].id, "ce-7");
    }
}
