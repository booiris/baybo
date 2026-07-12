mod error;
mod scheduler;
mod shutdown;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tools;

pub use baybo_model::{
    CronExecution, CronJob, CronSchedule, CronStatus, ExecutionOutcome, ExecutionStatus,
    PendingCronResult,
};
pub use baybo_store::{CronStore, ExecutionCompletion};
pub use error::CronError;
pub use scheduler::{CronScheduler, CronTriggerEvent, NewCronJob};
pub use shutdown::Shutdown;

#[cfg(any(test, feature = "test-support"))]
pub use shutdown::NeverShutdown;

pub type Result<T> = std::result::Result<T, CronError>;
