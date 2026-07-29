use async_trait::async_trait;
use baybo_model::{
    CronExecution, CronJob, CronStatus, ExecutionOutcome, ExecutionStatus, SessionId,
};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Persistence interface for cron jobs and executions. Implemented by
/// `baybo_storage::sqlite::SqliteCronStore`. The trait talks in the domain
/// types (`CronJob`, `CronExecution`, `ExecutionStatus`) from
/// `baybo-model`; the sqlite impl handles JSON round-tripping internally.
///
/// Deletion is a recycle bin: [`CronStore::delete`] stamps
/// `CronJob::deleted_at` and the row lives on. Every listing below returns
/// **live jobs only** — the filter is applied in SQL, so a deleted job can
/// never reach the tick loop or a user's list. [`CronStore::get`] is the one
/// exception: it resolves a deleted job by id, which is what keeps its
/// execution rows and the conversations they opened pointing at a real job.
#[async_trait]
pub trait CronStore: Send + Sync {
    // ── Turn CRUD ──
    async fn create(&self, job: &CronJob) -> Result<()>;
    /// Fetch a job by id, deleted or not.
    async fn get(&self, job_id: &str) -> Result<Option<CronJob>>;
    /// Persist a job's schedule and lifecycle — everything *except* its
    /// recycle-bin state, which only [`CronStore::delete`] and
    /// [`CronStore::restore`] may move. The tick loop reads a job, works, then
    /// writes it back; a `deleted_at` that this write could carry would let
    /// that stale snapshot undo a deletion that landed in between, putting a
    /// job the user deleted back on the schedule.
    async fn save(&self, job: &CronJob) -> Result<()>;
    /// Save a job, but only while the stored row is still `expected` — the
    /// snapshot the caller read: still live, still carrying its `status` and
    /// `next_trigger_at`, and still stamped with its `updated_at`. Returns false
    /// when the row moved and the write was dropped.
    ///
    /// This is the write every in-place change of a job goes through, because
    /// each of them rewrites the **whole** record from a snapshot: an edit, a
    /// pause, a resume. Any of those can be raced by a fire's write-back or by
    /// another change, and an unconditional `save` would then put the snapshot
    /// back — reverting the prompt the user just typed, re-arming a slot that
    /// already ran, forgetting `last_triggered_at`. The caller reloads and
    /// re-applies its change to the row as it now stands.
    ///
    /// `updated_at` is what makes this a real version check rather than a
    /// schedule check: a change that touches only authored fields moves neither
    /// column, so two of them would otherwise both "match" the same snapshot and
    /// the second would silently overwrite the first.
    async fn save_if_unchanged(&self, job: &CronJob, expected: &CronJob) -> Result<bool>;
    /// Stamp a fire onto its job: when it fired, the slot it goes to next, and
    /// the status it is left in.
    ///
    /// The fire's fields are applied to the **stored** row, not to `expected` —
    /// the snapshot the tick loop read before the fire. Those two differ exactly
    /// when a user edited the job while the slot was firing, and a write-back
    /// carrying the whole snapshot would revert that edit's prompt, title, or
    /// schedule. A fire owns when it ran and where the schedule goes next; it
    /// owns nothing the user typed.
    ///
    /// Conditional on the row still being the job the fire read (same `status`
    /// and `next_trigger_at`, still live), so a pause, a delete, or a reschedule
    /// that lands mid-fire wins: none of them may be re-armed away by the fire
    /// they raced. Returns false when the write was dropped for that reason.
    async fn record_fire(&self, expected: &CronJob, fire: CronFire) -> Result<bool>;
    /// Pin/unpin the job's **cron group** (`docs/cron-groups.md`). A targeted
    /// write on the flat `pinned` column — NOT through `save` /
    /// `save_if_unchanged` / `record_fire`, all of which rewrite the whole row
    /// from a snapshot the caller holds (and `record_fire` re-serializes it on
    /// every fire), so a pin routed through them would be reverted by the next
    /// tick. The same flat-column discipline `deleted_at` uses. Returns `false`
    /// when no such job exists.
    async fn set_pinned(&self, job_id: &str, pinned: bool) -> Result<bool>;
    /// Move a job to the recycle bin by stamping `deleted_at`. Leaves
    /// `status` untouched. Idempotent: a job already in the bin keeps the
    /// deletion time it went in with.
    async fn delete(&self, job_id: &str) -> Result<()>;
    /// Bring a job back from the recycle bin by clearing `deleted_at`.
    /// Leaves `status` and `next_trigger_at` untouched — the caller is
    /// responsible for having already written a schedule that is safe to
    /// publish (see `CronScheduler::restore_job`).
    async fn restore(&self, job_id: &str) -> Result<()>;
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<CronJob>>;
    /// Return every live cron job regardless of user or status. Ordering
    /// is unspecified — callers sort as needed.
    async fn list_all(&self) -> Result<Vec<CronJob>>;
    async fn list_enabled(&self) -> Result<Vec<CronJob>>;
    /// The recycle bin: every soft-deleted job, most recently deleted first.
    async fn list_deleted(&self) -> Result<Vec<CronJob>>;
    /// Return all enabled, live jobs whose `next_trigger_at` is at or before
    /// `now_us` (Unix microseconds).
    async fn list_due(&self, now_us: i64) -> Result<Vec<CronJob>>;

    // ── Execution records ──

    /// Persist a fresh execution. Returns [`StorageError::Conflict`] when
    /// two scheduler instances race on the same `(job_id,
    /// scheduled_fire_time)` slot — the unique index rejected the loser's
    /// insert. The tick path treats this as benign and skips the
    /// duplicate dispatch.
    async fn record_execution(&self, exec: &CronExecution) -> Result<()>;
    async fn list_executions_by_job(&self, job_id: &str) -> Result<Vec<CronExecution>>;
    async fn list_executions_by_user(&self, user_id: &str) -> Result<Vec<CronExecution>>;

    /// Check if an execution already exists for this (job_id,
    /// scheduled_fire_time) pair. Used for idempotent tick: prevents
    /// duplicate triggers for the same schedule slot.
    async fn has_execution_for_schedule(
        &self,
        job_id: &str,
        scheduled_fire_time_us: i64,
    ) -> Result<bool>;

    /// Transition an execution to a new status.
    async fn update_execution_status(
        &self,
        execution_id: &str,
        status: ExecutionStatus,
    ) -> Result<()>;

    /// List all execution records with the given status. Used at startup
    /// to find `Pending` executions that need re-dispatch.
    async fn list_executions_by_status(
        &self,
        status: ExecutionStatus,
    ) -> Result<Vec<CronExecution>>;

    // ── Delivery ledger (one-shot result → origin conversation) ──

    /// Stamp the fire's terminal state onto the execution: which session it
    /// ran in, how it ended, the ordinal of its reply row, and when. Written
    /// by the cron waiter **before** it routes the result, so a crash between
    /// here and the origin's append leaves a durable record for the boot
    /// re-drive to replay.
    async fn record_execution_completion(
        &self,
        execution_id: &str,
        completion: ExecutionCompletion,
    ) -> Result<()>;

    /// Mark this execution's delivery **resolved** — the result was appended
    /// to the origin conversation, or terminally dropped (no usable origin).
    /// Both stamp `notified_at`, so the re-drive scan converges either way.
    async fn mark_execution_notified(
        &self,
        execution_id: &str,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()>;

    /// Executions whose fire completed but whose result has not been
    /// delivered or dropped ([`CronExecution::awaits_delivery`]). Scanned at
    /// boot to re-drive deliveries lost to a crash.
    async fn list_executions_awaiting_delivery(&self) -> Result<Vec<CronExecution>>;
}

/// What a fire writes back to its [`CronJob`] — see [`CronStore::record_fire`].
/// Exactly the fields a fire owns: nothing a user authored is in here, which is
/// what lets a write-back and an in-place edit land in either order.
#[derive(Debug, Clone)]
pub struct CronFire {
    /// The status the fire leaves the job in: unchanged for a recurring job,
    /// [`CronStatus::Executed`] for a one-shot that has now run.
    pub status: CronStatus,
    /// The slot the job goes to next. `None` for a one-shot with nothing left
    /// to fire.
    pub next_trigger_at: Option<DateTime<Utc>>,
    pub last_triggered_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The fire's terminal state, stamped onto its [`CronExecution`] by the cron
/// waiter. Grouped into a struct so the store trait doesn't take five
/// positional arguments whose types (two `Option`s, a session id) are easy to
/// transpose at a call site.
#[derive(Debug, Clone)]
pub struct ExecutionCompletion {
    pub fire_session_id: SessionId,
    pub outcome: ExecutionOutcome,
    pub reply_ordinal: Option<i64>,
    pub completed_at: chrono::DateTime<chrono::Utc>,
}
