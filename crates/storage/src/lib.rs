pub mod retry;
pub mod sqlite;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

// Trait objects named by the `Store` DI bundle below. The contracts and
// their row/DTO types live in the `baybo-store` ports crate; consumers
// import them from `baybo_store` directly, not via this adapter.
use baybo_store::{
    AgentProfileStore, BlobStore, ChannelBotStore, ChannelPairingStore, ChannelSessionStore,
    CostStore, CronStore, DeviceStore, JobStore, MessageSearchStore, SecretStore,
    SessionFolderStore, SessionStore, SessionSummaryStore, SkillRiskStore, TaskStore, TraceStore,
};

/// Bundles all store implementations into a single container
/// for dependency injection by the assembly layer.
///
/// Each field is an `Arc<dyn XxxStore>` so the whole struct is cheap to
/// clone and hand to multiple consumers (manager graph, gateway server,
/// CLI helpers) without forcing each caller to maintain its own bag of
/// store fields.
#[derive(Clone)]
pub struct Store {
    pub session: std::sync::Arc<dyn SessionStore>,
    pub message_search: std::sync::Arc<dyn MessageSearchStore>,
    pub session_summary: std::sync::Arc<dyn SessionSummaryStore>,
    pub session_folder: std::sync::Arc<dyn SessionFolderStore>,
    pub task: std::sync::Arc<dyn TaskStore>,
    pub trace: std::sync::Arc<dyn TraceStore>,
    pub secret: std::sync::Arc<dyn SecretStore>,
    pub cost: std::sync::Arc<dyn CostStore>,
    pub job: std::sync::Arc<dyn JobStore>,
    pub cron: std::sync::Arc<dyn CronStore>,
    pub risk: std::sync::Arc<dyn SkillRiskStore>,
    pub channel_session: std::sync::Arc<dyn ChannelSessionStore>,
    pub channel_bot: std::sync::Arc<dyn ChannelBotStore>,
    pub channel_pairing: std::sync::Arc<dyn ChannelPairingStore>,
    pub device: std::sync::Arc<dyn DeviceStore>,
    pub agent_profile: std::sync::Arc<dyn AgentProfileStore>,
    pub blob: std::sync::Arc<dyn BlobStore>,
}

impl Store {
    /// Open (or create) a `Store` backed by a sqlite database at `path`.
    /// Parent directories are created if missing. Blob payloads land in
    /// `<parent>/blobs/` — kept alongside the sqlite db so a single
    /// state directory holds the whole persistent state.
    pub async fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let parent_dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                std::fs::create_dir_all(parent).map_err(|e| {
                    anyhow::anyhow!(
                        "failed to create storage directory {}: {e}",
                        parent.display()
                    )
                })?;
                parent.to_path_buf()
            }
            // Bare filename or root path: park blobs next to CWD; rare
            // outside tests that pass `:memory:`-style placeholders.
            _ => std::path::PathBuf::from("."),
        };
        let blob_root = parent_dir.join("blobs");
        let pool = sqlite::SqlitePool::open(path).await?;
        let blob = sqlite::SqliteBlobStore::open(pool.clone(), &blob_root).await?;
        let agent_profile = sqlite::SqliteAgentProfileStore::open(pool.clone()).await?;
        Ok(Self {
            session: std::sync::Arc::new(sqlite::SqliteSessionStore::new(pool.clone())),
            message_search: std::sync::Arc::new(sqlite::SqliteMessageSearchStore::new(
                pool.clone(),
            )),
            session_summary: std::sync::Arc::new(sqlite::SqliteSessionSummaryStore::new(
                pool.clone(),
            )),
            session_folder: std::sync::Arc::new(sqlite::SqliteSessionFolderStore::new(
                pool.clone(),
            )),
            task: std::sync::Arc::new(sqlite::SqliteTaskStore::new(pool.clone())),
            trace: std::sync::Arc::new(sqlite::SqliteTraceStore::new(pool.clone())),
            secret: std::sync::Arc::new(sqlite::SqliteSecretStore::new(pool.clone())),
            cost: std::sync::Arc::new(sqlite::SqliteCostStore::new(pool.clone())),
            job: std::sync::Arc::new(sqlite::SqliteJobStore::new(pool.clone())),
            cron: std::sync::Arc::new(sqlite::SqliteCronStore::new(pool.clone())),
            risk: std::sync::Arc::new(sqlite::SqliteSkillRiskStore::new(pool.clone())),
            channel_session: std::sync::Arc::new(sqlite::SqliteChannelSessionStore::new(
                pool.clone(),
            )),
            channel_bot: std::sync::Arc::new(sqlite::SqliteChannelBotStore::new(pool.clone())),
            channel_pairing: std::sync::Arc::new(sqlite::SqliteChannelPairingStore::new(
                pool.clone(),
            )),
            device: std::sync::Arc::new(sqlite::SqliteDeviceStore::new(pool.clone())),
            agent_profile: std::sync::Arc::new(agent_profile),
            blob: std::sync::Arc::new(blob),
        })
    }
}
