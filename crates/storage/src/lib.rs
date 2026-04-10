pub mod cost;
pub mod error;
pub mod job;
pub mod libsql;
pub mod memory;
pub mod secret;
pub mod trace;

use aura_session::SessionStore;

pub use cost::{CostError, CostRecord, CostResult, CostStore, CostSummary, TimeRange};
pub use error::StorageError;
pub use job::JobStore;
pub use memory::MemoryStore;
pub use secret::SecretStore;
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
}

impl Store {
    /// Create a `Store` backed entirely by in-memory libsql stores.
    pub async fn in_memory() -> anyhow::Result<Self> {
        let pool = libsql::LibsqlPool::open_in_memory().await?;
        Ok(Self {
            session: Box::new(libsql::LibsqlSessionStore::new(pool.clone())),
            memory: Box::new(libsql::LibsqlMemoryStore::new(pool.clone())),
            trace: Box::new(libsql::LibsqlTraceStore::new(pool.clone())),
            secret: Box::new(libsql::LibsqlSecretStore::new(pool.clone())),
            cost: Box::new(libsql::LibsqlCostStore::new(pool.clone())),
            job: Box::new(libsql::LibsqlJobStore::new(pool)),
        })
    }
}
