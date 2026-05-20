mod error;
mod scheduler;
mod shutdown;

pub use aura_model::{CronExecution, CronJob, CronSchedule, CronStatus, ExecutionStatus};
pub use aura_store::CronStore;
pub use error::CronError;
pub use scheduler::{CronScheduler, CronTriggerEvent};
pub use shutdown::Shutdown;

#[cfg(any(test, feature = "test-support"))]
pub use shutdown::NeverShutdown;

pub type Result<T> = std::result::Result<T, CronError>;
