pub mod memory_backend;
pub mod sqlite;

use aura_cost::CostStore;
use aura_job::JobStore;
use aura_memory::MemoryStore;
use aura_security::SecretStore;
use aura_session::SessionStore;
use aura_trace::TraceStore;

/// Bundles all store implementations into a single container
/// for dependency injection by the assembly layer.
pub struct StorageSet {
    pub session: Box<dyn SessionStore>,
    pub memory: Box<dyn MemoryStore>,
    pub trace: Box<dyn TraceStore>,
    pub secret: Box<dyn SecretStore>,
    pub cost: Box<dyn CostStore>,
    pub job: Box<dyn JobStore>,
}

impl StorageSet {
    /// Create a `StorageSet` backed entirely by in-memory stores.
    pub fn in_memory() -> Self {
        Self {
            session: Box::new(memory_backend::InMemorySessionStore::new()),
            memory: Box::new(memory_backend::InMemoryMemoryStore::new()),
            trace: Box::new(memory_backend::InMemoryTraceStore::new()),
            secret: Box::new(memory_backend::InMemorySecretStore::new()),
            cost: Box::new(memory_backend::InMemoryCostStore::new()),
            job: Box::new(memory_backend::InMemoryJobStore::new()),
        }
    }
}
