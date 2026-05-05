mod error;
mod job;
mod scheduler;
mod shutdown;
mod tools;

pub use error::CronError;
pub use job::{
    CronExecution, CronJob, CronSchedule, CronStatus, DEFAULT_TIMEZONE, ExecutionStatus,
    TriggerAction,
};
pub use scheduler::{CronScheduler, CronTriggerEvent};
pub use shutdown::Shutdown;
pub use tools::agent_tools;

#[cfg(any(test, feature = "test-support"))]
pub use shutdown::NeverShutdown;

pub type Result<T> = std::result::Result<T, CronError>;
