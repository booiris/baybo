#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("cron job not found: {0}")]
    NotFound(String),
    #[error("invalid schedule: {0}")]
    InvalidSchedule(String),
    /// An in-place edit whose patch sets no field. Always a caller bug: there is
    /// nothing to write, and reporting success would tell the user their job
    /// changed when nothing about it did.
    #[error("cron job update sets no fields: {0}")]
    EmptyUpdate(String),
    /// A create or edit that would leave the job with nothing to say. Not a job
    /// that does nothing: a job that keeps its schedule and fires an empty
    /// instruction on every slot.
    #[error("cron job prompt is blank: a scheduled job has to carry an instruction")]
    BlankPrompt,
    /// Every attempt at an in-place edit lost to a concurrent write — a fire's
    /// write-back, another edit. The row is intact and the caller can retry.
    #[error("cron job {0} kept changing under the edit; try again")]
    Contended(String),
    /// Two scheduler instances raced on the same `(job_id,
    /// scheduled_fire_time)` slot — the unique index rejected the
    /// loser's insert. The tick path treats this as benign and skips
    /// the duplicate dispatch.
    #[error("cron execution already dispatched: {0}")]
    AlreadyDispatched(String),
    /// A delete aimed at a job the runtime owns and seeds (the dream pass).
    /// It has a switch in `baybo.json` and a pause button; deleting it would
    /// leave nothing to recreate the user's schedule for it.
    #[error("cron job {0} is built in: pause it, or switch the feature off in config")]
    Builtin(String),
    #[error("cron storage error: {0}")]
    Storage(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<baybo_store::StorageError> for CronError {
    fn from(e: baybo_store::StorageError) -> Self {
        match e {
            baybo_store::StorageError::Conflict(s) => CronError::AlreadyDispatched(s),
            baybo_store::StorageError::NotFound(s) => CronError::NotFound(s),
            baybo_store::StorageError::Internal(e) => CronError::Internal(e),
            other => CronError::Storage(other.to_string()),
        }
    }
}
