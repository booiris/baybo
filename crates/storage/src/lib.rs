pub mod memory_backend;
pub mod sqlite;

use aura_core::Result;
use aura_cost::CostStore;
use aura_job::JobStore;
use aura_memory::MemoryStore;
use aura_security::SecretStore;
use aura_session::SessionStore;
use aura_trace::TraceStore;
use serde::{Deserialize, Serialize};

/// Which persistence backend to use.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Memory,
    Sqlite,
}

/// Configuration for the storage layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub backend: Backend,
    pub sqlite_path: Option<String>,
}

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

    /// Create a `StorageSet` backed by SQLite at the given file path.
    pub fn sqlite(path: &str) -> Result<Self> {
        let pool = crate::sqlite::SqlitePool::open(path)?;
        Ok(Self {
            session: Box::new(crate::sqlite::SqliteSessionStore::new(pool.clone())),
            memory: Box::new(crate::sqlite::SqliteMemoryStore::new(pool.clone())),
            trace: Box::new(crate::sqlite::SqliteTraceStore::new(pool.clone())),
            secret: Box::new(crate::sqlite::SqliteSecretStore::new(pool.clone())),
            cost: Box::new(crate::sqlite::SqliteCostStore::new(pool.clone())),
            job: Box::new(crate::sqlite::SqliteJobStore::new(pool)),
        })
    }
}

/// Factory that creates a [`StorageSet`] from configuration.
pub struct StorageFactory;

impl StorageFactory {
    pub fn create(config: &StorageConfig) -> Result<StorageSet> {
        match config.backend {
            Backend::Memory => Ok(StorageSet::in_memory()),
            Backend::Sqlite => {
                let path = config.sqlite_path.as_deref().unwrap_or("aura.db");
                StorageSet::sqlite(path)
            }
        }
    }
}
