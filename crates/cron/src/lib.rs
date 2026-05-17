mod error;
mod job;
mod scheduler;
mod shutdown;
mod store;

pub use error::CronError;
pub use job::{CronExecution, CronJob, CronSchedule, CronStatus, ExecutionStatus};
pub use scheduler::{CronScheduler, CronTriggerEvent};
pub use shutdown::Shutdown;
pub use store::CronStore;

#[cfg(any(test, feature = "test-support"))]
pub use shutdown::NeverShutdown;

pub type Result<T> = std::result::Result<T, CronError>;
