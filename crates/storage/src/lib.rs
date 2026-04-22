pub mod channel_bot;
pub mod channel_session;
pub mod cost;
pub mod cron;
pub mod error;
pub mod job;
pub mod libsql;
pub mod memory;
pub mod retry;
pub mod secret;
pub mod session;
pub mod skill_risk;
pub mod trace;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use channel_bot::{ChannelBotRow, ChannelBotStore};
pub use channel_session::ChannelSessionStore;
pub use cost::{CostError, CostRecord, CostResult, CostStore, CostSummary, TimeRange};
pub use cron::{CronExecutionRow, CronJobRow, CronStore, CronStoreError};
pub use error::StorageError;
pub use job::JobStore;
pub use memory::MemoryStore;
pub use retry::retry_on_busy;
pub use secret::SecretStore;
pub use session::SessionStore;
pub use skill_risk::{AssessmentJob, AssessmentJobStatus, RiskLevel, RiskVerdict, SkillRiskStore};
pub use trace::TraceStore;

/// Bundles all store implementations into a single container
/// for dependency injection by the assembly layer.
pub struct Store {
    pub session: Box<dyn SessionStore>,
    pub memory: Box<dyn MemoryStore>,
    pub trace: Box<dyn TraceStore>,
    pub secret: Box<dyn SecretStore>,
    pub cost: Box<dyn CostStore>,
    pub job: Box<dyn JobStore>,
    pub cron: Box<dyn CronStore>,
    pub risk: Box<dyn SkillRiskStore>,
    pub channel_session: Box<dyn ChannelSessionStore>,
    pub channel_bot: Box<dyn ChannelBotStore>,
}

impl Store {
    /// Open (or create) a `Store` backed by a libsql database at `path`.
    /// Parent directories are created if missing.
    pub async fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!(
                    "failed to create storage directory {}: {e}",
                    parent.display()
                )
            })?;
        }
        let pool = libsql::LibsqlPool::open(path).await?;
        Ok(Self {
            session: Box::new(libsql::LibsqlSessionStore::new(pool.clone())),
            memory: Box::new(libsql::LibsqlMemoryStore::new(pool.clone())),
            trace: Box::new(libsql::LibsqlTraceStore::new(pool.clone())),
            secret: Box::new(libsql::LibsqlSecretStore::new(pool.clone())),
            cost: Box::new(libsql::LibsqlCostStore::new(pool.clone())),
            job: Box::new(libsql::LibsqlJobStore::new(pool.clone())),
            cron: Box::new(libsql::LibsqlCronStore::new(pool.clone())),
            risk: Box::new(libsql::LibsqlSkillRiskStore::new(pool.clone())),
            channel_session: Box::new(libsql::LibsqlChannelSessionStore::new(pool.clone())),
            channel_bot: Box::new(libsql::LibsqlChannelBotStore::new(pool)),
        })
    }
}
