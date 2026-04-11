#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("cron job not found: {0}")]
    NotFound(String),
    #[error("invalid cron expression: {0}")]
    InvalidExpression(String),
    #[error("cron storage error: {0}")]
    Storage(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
