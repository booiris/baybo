#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("cron job not found: {0}")]
    NotFound(String),
    #[error("invalid schedule: {0}")]
    InvalidSchedule(String),
    #[error("cron storage error: {0}")]
    Storage(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
