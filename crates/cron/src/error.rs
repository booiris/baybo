#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("cron job not found: {0}")]
    NotFound(String),
    #[error("invalid schedule: {0}")]
    InvalidSchedule(String),
    /// Two scheduler instances raced on the same `(job_id,
    /// scheduled_fire_time)` slot — the unique index rejected the
    /// loser's insert. The tick path treats this as benign and skips
    /// the duplicate dispatch.
    #[error("cron execution already dispatched: {0}")]
    AlreadyDispatched(String),
    #[error("cron storage error: {0}")]
    Storage(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<aura_store::StorageError> for CronError {
    fn from(e: aura_store::StorageError) -> Self {
        match e {
            aura_store::StorageError::Conflict(s) => CronError::AlreadyDispatched(s),
            aura_store::StorageError::NotFound(s) => CronError::NotFound(s),
            aura_store::StorageError::Internal(e) => CronError::Internal(e),
            other => CronError::Storage(other.to_string()),
        }
    }
}
