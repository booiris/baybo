mod error;
mod scheduler;
mod shutdown;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod tools;

pub use baybo_model::{
    CronExecution, CronJob, CronJobPatch, CronSchedule, CronStatus, ExecutionOutcome,
    ExecutionStatus, PendingCronResult,
};
pub use baybo_store::{CronFire, CronStore, ExecutionCompletion};
pub use error::CronError;
pub use scheduler::{BuiltinJobSpec, CronScheduler, CronTriggerEvent, NewCronJob, host_timezone};
pub use shutdown::Shutdown;

#[cfg(any(test, feature = "test-support"))]
pub use shutdown::NeverShutdown;

pub type Result<T> = std::result::Result<T, CronError>;
